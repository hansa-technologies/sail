/// Spark-compatible `zip_with(left, right, (x, y) -> ...)` higher-order function.
///
/// Spark semantics (`ZipWith` in
/// `sql/catalyst/src/main/scala/org/apache/spark/sql/catalyst/expressions/higherOrderFunctions.scala`):
/// the result has `max(size(left), size(right))` elements per row, the shorter
/// array is padded with NULL, the result is NULL when either array is NULL, and
/// the result element type is the lambda's return type. The lambda is never
/// evaluated for a row whose result is NULL.
use std::sync::Arc;

use datafusion::arrow::array::{Array, ArrayRef, ListArray, UInt64Array};
use datafusion::arrow::buffer::{NullBuffer, OffsetBuffer};
use datafusion::arrow::compute::{cast, take, take_arrays};
use datafusion::arrow::datatypes::{DataType, Field, FieldRef};
use datafusion_common::{Result, exec_err, plan_err};
use datafusion_expr::{
    ColumnarValue, HigherOrderFunctionArgs, HigherOrderReturnFieldArgs, HigherOrderSignature,
    HigherOrderUDFImpl, LambdaParametersProgress, ValueOrLambda, Volatility,
};

/// Like [`super::spark_array_transform::SparkArrayTransform`], the physical
/// lambda evaluation batch is laid out as `[captures..., params...]` with the
/// body projected to the columns it actually uses, which only lines up when the
/// used parameters form a prefix of the declared ones. A lambda that uses only
/// its second parameter, `(x, y) -> f(y)`, is therefore rewritten by the planner
/// to `y -> f(y)` over a [`SparkArrayZipWith::new_swapped`] instance, whose
/// lambda parameters are `[right, left]`.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct SparkArrayZipWith {
    signature: HigherOrderSignature,
    swapped: bool,
}

impl Default for SparkArrayZipWith {
    fn default() -> Self {
        Self::new()
    }
}

impl SparkArrayZipWith {
    pub fn new() -> Self {
        Self::with_order(false)
    }

    pub fn new_swapped() -> Self {
        Self::with_order(true)
    }

    fn with_order(swapped: bool) -> Self {
        Self {
            signature: HigherOrderSignature::exact(
                vec![
                    ValueOrLambda::Value(()),
                    ValueOrLambda::Value(()),
                    ValueOrLambda::Lambda(()),
                ],
                Volatility::Immutable,
            ),
            swapped,
        }
    }

    /// Whether the lambda parameters are `[right, left]`. Used by the
    /// serialization codec to reconstruct the correct variant.
    pub fn is_swapped(&self) -> bool {
        self.swapped
    }
}

fn zip_with_args<V: std::fmt::Debug, L: std::fmt::Debug>(
    args: &[ValueOrLambda<V, L>],
) -> Result<(&V, &V, &L)> {
    match args {
        [
            ValueOrLambda::Value(left),
            ValueOrLambda::Value(right),
            ValueOrLambda::Lambda(lambda),
        ] => Ok((left, right, lambda)),
        _ => plan_err!("zip_with expects two arrays followed by a lambda, got {args:?}"),
    }
}

fn element_field(name: &str, data_type: &DataType) -> Result<FieldRef> {
    match data_type {
        DataType::List(field) | DataType::LargeList(field) => {
            // Padding makes the lambda parameters nullable even for a
            // non-nullable element type.
            Ok(Arc::new(
                Field::new("", field.data_type().clone(), true)
                    .with_metadata(field.metadata().clone()),
            ))
        }
        other => plan_err!("{name} expected a list, got {other}"),
    }
}

impl HigherOrderUDFImpl for SparkArrayZipWith {
    fn name(&self) -> &str {
        "zip_with"
    }

    fn signature(&self) -> &HigherOrderSignature {
        &self.signature
    }

    fn lambda_parameters(
        &self,
        _step: usize,
        fields: &[ValueOrLambda<FieldRef, Option<FieldRef>>],
    ) -> Result<LambdaParametersProgress> {
        let (left, right, _lambda) = zip_with_args(fields)?;
        let left = element_field(self.name(), left.data_type())?;
        let right = element_field(self.name(), right.data_type())?;
        let params = if self.swapped {
            vec![right, left]
        } else {
            vec![left, right]
        };
        Ok(LambdaParametersProgress::Complete(vec![params]))
    }

    fn return_field_from_args(&self, args: HigherOrderReturnFieldArgs) -> Result<FieldRef> {
        let (left, right, lambda) = zip_with_args(args.arg_fields)?;
        for list in [left, right] {
            if !matches!(list.data_type(), DataType::List(_)) {
                return plan_err!("{} expected a list, got {}", self.name(), list.data_type());
            }
        }
        let field = Arc::new(Field::new(
            Field::LIST_FIELD_DEFAULT_NAME,
            lambda.data_type().clone(),
            lambda.is_nullable(),
        ));
        Ok(Arc::new(Field::new(
            "",
            DataType::List(field),
            left.is_nullable() || right.is_nullable(),
        )))
    }

    fn invoke_with_args(&self, args: HigherOrderFunctionArgs) -> Result<ColumnarValue> {
        let (left, right, lambda) = zip_with_args(&args.args)?;
        let left = left.to_array(args.number_rows)?;
        let right = right.to_array(args.number_rows)?;
        let (Some(left), Some(right)) = (
            left.as_any().downcast_ref::<ListArray>(),
            right.as_any().downcast_ref::<ListArray>(),
        ) else {
            return exec_err!("zip_with expected two List arrays");
        };
        let field = match args.return_field.data_type() {
            DataType::List(field) => Arc::clone(field),
            other => return exec_err!("zip_with expected a list return field, got {other}"),
        };

        let rows = left.len();
        let nulls = NullBuffer::union(left.nulls(), right.nulls());
        let (left_offsets, right_offsets) = (left.offsets(), right.offsets());
        let mut lengths = Vec::with_capacity(rows);
        let mut left_idx: Vec<Option<u64>> = Vec::new();
        let mut right_idx: Vec<Option<u64>> = Vec::new();
        let mut row_idx: Vec<u64> = Vec::new();
        for row in 0..rows {
            if nulls.as_ref().is_some_and(|n| n.is_null(row)) {
                lengths.push(0);
                continue;
            }
            let (ls, le) = (left_offsets[row] as usize, left_offsets[row + 1] as usize);
            let (rs, re) = (right_offsets[row] as usize, right_offsets[row + 1] as usize);
            let (ll, rl) = (le - ls, re - rs);
            let len = ll.max(rl);
            lengths.push(len);
            for j in 0..len {
                left_idx.push((j < ll).then_some((ls + j) as u64));
                right_idx.push((j < rl).then_some((rs + j) as u64));
                row_idx.push(row as u64);
            }
        }

        let total = row_idx.len();
        let offsets = OffsetBuffer::<i32>::from_lengths(lengths);
        if total == 0 {
            let values = datafusion::arrow::array::new_empty_array(field.data_type());
            return Ok(ColumnarValue::Array(Arc::new(ListArray::new(
                field, offsets, values, nulls,
            ))));
        }

        let x = take(left.values().as_ref(), &UInt64Array::from(left_idx), None)?;
        let y = take(right.values().as_ref(), &UInt64Array::from(right_idx), None)?;
        let row_idx = UInt64Array::from(row_idx);
        let x_param = || Ok(Arc::clone(&x));
        let y_param = || Ok(Arc::clone(&y));
        let params: [&dyn Fn() -> Result<ArrayRef>; 2] = if self.swapped {
            [&y_param, &x_param]
        } else {
            [&x_param, &y_param]
        };
        let values = lambda
            .evaluate(&params, |arrays| Ok(take_arrays(arrays, &row_idx, None)?))?
            .into_array(total)?;
        let values = if values.data_type() == field.data_type() {
            values
        } else {
            cast(values.as_ref(), field.data_type())?
        };
        Ok(ColumnarValue::Array(Arc::new(ListArray::new(
            field, offsets, values, nulls,
        ))))
    }

    fn coerce_value_types(&self, arg_types: &[DataType]) -> Result<Vec<DataType>> {
        if arg_types.len() != 2 {
            return plan_err!(
                "zip_with function requires 2 value arguments, got {}",
                arg_types.len()
            );
        }
        arg_types
            .iter()
            .map(|t| match t {
                DataType::List(_) => Ok(t.clone()),
                DataType::LargeList(f)
                | DataType::ListView(f)
                | DataType::LargeListView(f)
                | DataType::FixedSizeList(f, _) => Ok(DataType::List(Arc::clone(f))),
                other => plan_err!("zip_with expected a list as array argument, got {other}"),
            })
            .collect()
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;

    use datafusion::arrow::array::{AsArray, Int32Array, RecordBatch};
    use datafusion_common::DFSchema;
    use datafusion_expr::execution_props::ExecutionProps;
    use datafusion_expr::expr::{HigherOrderFunction, LambdaVariable};
    use datafusion_expr::physical_planning_context::PhysicalPlanningContext;
    use datafusion_expr::{Expr, HigherOrderUDF, col, lambda};
    use datafusion_physical_expr::create_physical_expr;

    use super::*;

    fn list(rows: Vec<Option<Vec<Option<i32>>>>) -> ListArray {
        ListArray::from_iter_primitive::<datafusion::arrow::datatypes::Int32Type, _, _>(rows)
    }

    fn var(name: &str) -> Expr {
        Expr::LambdaVariable(LambdaVariable::new(
            name.to_string(),
            Some(Arc::new(Field::new(name, DataType::Int32, true))),
        ))
    }

    fn eval(
        func: SparkArrayZipWith,
        params: &[&str],
        body: Expr,
        l: ListArray,
        r: ListArray,
    ) -> ListArray {
        let schema = DFSchema::from_unqualified_fields(
            vec![
                Field::new("l", l.data_type().clone(), true),
                Field::new("r", r.data_type().clone(), true),
            ]
            .into(),
            HashMap::new(),
        )
        .unwrap();
        let n = l.len();
        let expr = Expr::HigherOrderFunction(HigherOrderFunction::new(
            Arc::new(HigherOrderUDF::new_from_impl(func)),
            vec![col("l"), col("r"), lambda(params.iter().copied(), body)],
        ));
        let out = create_physical_expr(
            &expr,
            &schema,
            &ExecutionProps::new(),
            &PhysicalPlanningContext::default(),
        )
        .unwrap()
        .evaluate(
            &RecordBatch::try_new(Arc::clone(schema.inner()), vec![Arc::new(l), Arc::new(r)])
                .unwrap(),
        )
        .unwrap()
        .into_array(n)
        .unwrap();
        out.as_list::<i32>().clone()
    }

    fn rows(a: &ListArray) -> Vec<Option<Vec<Option<i32>>>> {
        (0..a.len())
            .map(|i| {
                a.is_valid(i).then(|| {
                    a.value(i)
                        .as_any()
                        .downcast_ref::<Int32Array>()
                        .unwrap()
                        .iter()
                        .collect()
                })
            })
            .collect()
    }

    #[test]
    fn pads_shorter_array_with_null_and_propagates_null_arrays() {
        let l = list(vec![
            Some(vec![Some(1), Some(2), Some(3)]),
            Some(vec![Some(1)]),
            None,
            Some(vec![]),
        ]);
        let r = list(vec![
            Some(vec![Some(4), Some(5), Some(6)]),
            Some(vec![Some(10), Some(20), Some(30)]),
            Some(vec![Some(1)]),
            Some(vec![]),
        ]);
        let out = eval(
            SparkArrayZipWith::new(),
            &["x", "y"],
            var("x") + var("y"),
            l,
            r,
        );
        assert_eq!(
            rows(&out),
            vec![
                Some(vec![Some(5), Some(7), Some(9)]),
                Some(vec![Some(11), None, None]),
                None,
                Some(vec![]),
            ]
        );
    }

    #[test]
    fn swapped_instance_supports_second_parameter_only() {
        let l = list(vec![Some(vec![Some(1), Some(2)])]);
        let r = list(vec![Some(vec![Some(7), Some(8), Some(9)])]);
        let out = eval(SparkArrayZipWith::new_swapped(), &["y"], var("y"), l, r);
        assert_eq!(rows(&out), vec![Some(vec![Some(7), Some(8), Some(9)])]);
    }
}
