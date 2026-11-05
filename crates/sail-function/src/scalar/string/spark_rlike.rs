use std::sync::Arc;

use datafusion::arrow::array::{Array, ArrayRef, BooleanBuilder, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, FieldRef};
use datafusion_common::Result;
use datafusion_common::utils::take_function_args;
use datafusion_expr::function::Hint;
use datafusion_expr::{ReturnFieldArgs, ScalarFunctionArgs, ScalarUDFImpl};
use datafusion_expr_common::columnar_value::ColumnarValue;
use datafusion_expr_common::signature::{Signature, Volatility};

use super::spark_regex::SparkRegex;
use crate::error::{generic_internal_err, unsupported_data_types_exec_err};
use crate::functions_utils::{StrMemo, make_scalar_function};

/// Spark `rlike(str, regexp)`: `java.util.regex.Matcher.find()` anywhere in `str`.
///
/// Uses the `regex` crate and falls back to `fancy-regex` for look-around and
/// back-references (see [`SparkRegex`]).
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct SparkRlike {
    signature: Signature,
}

impl Default for SparkRlike {
    fn default() -> Self {
        Self::new()
    }
}

impl SparkRlike {
    pub const NAME: &'static str = "rlike";

    pub fn new() -> Self {
        Self {
            signature: Signature::user_defined(Volatility::Immutable),
        }
    }
}

impl ScalarUDFImpl for SparkRlike {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::Boolean)
    }

    /// NULL when any argument is NULL, so a non-null input gives a non-nullable result, as in Spark.
    fn return_field_from_args(&self, args: ReturnFieldArgs) -> Result<FieldRef> {
        let nullable = args.arg_fields.iter().any(|f| f.is_nullable());
        Ok(Arc::new(Field::new(
            self.name(),
            DataType::Boolean,
            nullable,
        )))
    }

    fn coerce_types(&self, arg_types: &[DataType]) -> Result<Vec<DataType>> {
        if arg_types.len() != 2 {
            return Err(unsupported_data_types_exec_err(
                Self::NAME,
                "Expected (STRING, STRING)",
                arg_types,
            ));
        }
        Ok(vec![DataType::Utf8, DataType::Utf8])
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let ScalarFunctionArgs { args, .. } = args;
        make_scalar_function(rlike_inner, vec![Hint::Pad, Hint::AcceptsSingular])(&args)
    }
}

fn rlike_inner(args: &[ArrayRef]) -> Result<ArrayRef> {
    let [values_arr, pattern_arr] = take_function_args(SparkRlike::NAME, args)?;
    let (Some(values), Some(pattern)) = (
        values_arr.as_any().downcast_ref::<StringArray>(),
        pattern_arr.as_any().downcast_ref::<StringArray>(),
    ) else {
        return Err(generic_internal_err(
            SparkRlike::NAME,
            "Could not downcast arguments to string arrays",
        ));
    };

    let scalar_pattern = (pattern.len() == 1 && pattern.is_valid(0))
        .then(|| SparkRegex::new(SparkRlike::NAME, pattern.value(0)))
        .transpose()?;
    let mut memo = StrMemo::new();
    let mut builder = BooleanBuilder::with_capacity(values.len());
    for i in 0..values.len() {
        let pattern_null = if pattern.len() == 1 {
            pattern.is_null(0)
        } else {
            pattern.is_null(i)
        };
        if values.is_null(i) || pattern_null {
            builder.append_null();
            continue;
        }
        let re = memo.resolve(
            scalar_pattern.as_ref(),
            || pattern.value(i),
            |p| SparkRegex::new(SparkRlike::NAME, p),
        )?;
        builder.append_value(re.is_match(values.value(i))?);
    }
    Ok(Arc::new(builder.finish()))
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use datafusion::arrow::array::BooleanArray;

    use super::*;

    #[test]
    fn rlike_scalar_and_column_patterns() -> Result<()> {
        let values: ArrayRef = Arc::new(StringArray::from(vec![
            Some("foobar"),
            Some("foobaz"),
            Some("hello"),
            None,
        ]));
        let pattern: ArrayRef = Arc::new(StringArray::from(vec![Some("foo(?=bar)")]));
        let out = rlike_inner(&[Arc::clone(&values), pattern])?;
        let out = out.as_any().downcast_ref::<BooleanArray>().unwrap();
        assert_eq!(
            out.iter().collect::<Vec<_>>(),
            [Some(true), Some(false), Some(false), None]
        );

        let patterns: ArrayRef = Arc::new(StringArray::from(vec![
            Some(r"(\w)\1"),
            Some("z$"),
            None,
            Some("a"),
        ]));
        let out = rlike_inner(&[values, patterns])?;
        let out = out.as_any().downcast_ref::<BooleanArray>().unwrap();
        assert_eq!(
            out.iter().collect::<Vec<_>>(),
            [Some(true), Some(true), None, None]
        );
        Ok(())
    }
}
