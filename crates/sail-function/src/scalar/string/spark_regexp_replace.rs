use std::sync::Arc;

use datafusion::arrow::array::{Array, ArrayRef, StringArray, StringBuilder};
use datafusion::arrow::datatypes::{DataType, Field, FieldRef};
use datafusion_common::Result;
use datafusion_common::utils::take_function_args;
use datafusion_expr::function::Hint;
use datafusion_expr::{ReturnFieldArgs, ScalarFunctionArgs, ScalarUDFImpl};
use datafusion_expr_common::columnar_value::ColumnarValue;
use datafusion_expr_common::signature::{Signature, Volatility};

use super::spark_regex::{JavaReplacement, SparkRegex};
use crate::error::{generic_internal_err, unsupported_data_types_exec_err};
use crate::functions_utils::{StrMemo, make_scalar_function};

/// Spark `regexp_replace(str, regexp, rep)`: `Matcher.replaceAll(rep)` with
/// Java replacement syntax (`$n`, `\x`).
///
/// Uses the `regex` crate and falls back to `fancy-regex` for look-around and
/// back-references (see [`SparkRegex`]).
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct SparkRegexpReplace {
    signature: Signature,
}

impl Default for SparkRegexpReplace {
    fn default() -> Self {
        Self::new()
    }
}

impl SparkRegexpReplace {
    pub const NAME: &'static str = "regexp_replace";

    pub fn new() -> Self {
        Self {
            signature: Signature::user_defined(Volatility::Immutable),
        }
    }
}

impl ScalarUDFImpl for SparkRegexpReplace {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::Utf8)
    }

    /// NULL when any argument is NULL, so a non-null input gives a non-nullable result, as in Spark.
    fn return_field_from_args(&self, args: ReturnFieldArgs) -> Result<FieldRef> {
        let nullable = args.arg_fields.iter().any(|f| f.is_nullable());
        Ok(Arc::new(Field::new(self.name(), DataType::Utf8, nullable)))
    }

    fn coerce_types(&self, arg_types: &[DataType]) -> Result<Vec<DataType>> {
        if arg_types.len() != 3 {
            return Err(unsupported_data_types_exec_err(
                Self::NAME,
                "Expected (STRING, STRING, STRING)",
                arg_types,
            ));
        }
        Ok(vec![DataType::Utf8; 3])
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let ScalarFunctionArgs { args, .. } = args;
        make_scalar_function(
            regexp_replace_inner,
            vec![Hint::Pad, Hint::AcceptsSingular, Hint::AcceptsSingular],
        )(&args)
    }
}

fn regexp_replace_inner(args: &[ArrayRef]) -> Result<ArrayRef> {
    let [values_arr, pattern_arr, replacement_arr] =
        take_function_args(SparkRegexpReplace::NAME, args)?;
    let (Some(values), Some(pattern), Some(replacement)) = (
        values_arr.as_any().downcast_ref::<StringArray>(),
        pattern_arr.as_any().downcast_ref::<StringArray>(),
        replacement_arr.as_any().downcast_ref::<StringArray>(),
    ) else {
        return Err(generic_internal_err(
            SparkRegexpReplace::NAME,
            "Could not downcast arguments to string arrays",
        ));
    };

    let name = SparkRegexpReplace::NAME;
    let scalar_pattern = (pattern.len() == 1 && pattern.is_valid(0))
        .then(|| SparkRegex::new(name, pattern.value(0)))
        .transpose()?;
    // Only parsed up front when both are scalars; the group count comes from the pattern.
    let scalar_replacement = match (&scalar_pattern, replacement.len() == 1) {
        (Some(re), true) if replacement.is_valid(0) => {
            Some(JavaReplacement::parse(name, replacement.value(0), re)?)
        }
        _ => None,
    };

    let mut memo = StrMemo::new();
    let mut builder = StringBuilder::with_capacity(values.len(), values.len() * 8);
    for i in 0..values.len() {
        let p_idx = if pattern.len() == 1 { 0 } else { i };
        let r_idx = if replacement.len() == 1 { 0 } else { i };
        if values.is_null(i) || pattern.is_null(p_idx) || replacement.is_null(r_idx) {
            builder.append_null();
            continue;
        }
        let re = memo.resolve(
            scalar_pattern.as_ref(),
            || pattern.value(i),
            |p| SparkRegex::new(name, p),
        )?;
        let out = match &scalar_replacement {
            Some(rep) => re.replace_all(values.value(i), rep)?,
            None => {
                let rep = JavaReplacement::parse(name, replacement.value(r_idx), re)?;
                re.replace_all(values.value(i), &rep)?
            }
        };
        builder.append_value(out);
    }
    Ok(Arc::new(builder.finish()))
}

#[cfg(test)]
#[expect(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn run(values: Vec<Option<&str>>, pattern: &str, rep: &str) -> Vec<Option<String>> {
        let v: ArrayRef = Arc::new(StringArray::from(values));
        let p: ArrayRef = Arc::new(StringArray::from(vec![Some(pattern)]));
        let r: ArrayRef = Arc::new(StringArray::from(vec![Some(rep)]));
        let out = regexp_replace_inner(&[v, p, r]).unwrap();
        out.as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .iter()
            .map(|s| s.map(str::to_string))
            .collect()
    }

    #[test]
    fn lookaround_and_backreference() {
        assert_eq!(
            run(vec![Some("a1b22"), None], r"(?<=\d)(?=\D)", "-"),
            [Some("a1-b22".to_string()), None]
        );
        assert_eq!(
            run(vec![Some("aabbc")], r"(\w)\1", "<$1>"),
            ["<a><b>c"].map(|s| Some(s.to_string()))
        );
        assert_eq!(
            run(vec![Some("abc")], "b*", "-"),
            [Some("-a--c-".to_string())]
        );
    }
}
