use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString, PyTuple};
use serde_json::{Map, Value};

pub fn import<'py>(py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyModule>> {
    PyModule::import(py, name)
}

pub fn optional_import<'py>(py: Python<'py>, name: &str) -> Option<Bound<'py, PyModule>> {
    import(py, name).ok()
}

pub fn attr<'py>(object: &Bound<'py, PyAny>, name: &str) -> Option<Bound<'py, PyAny>> {
    object.getattr(name).ok()
}

pub fn call0<'py>(object: &Bound<'py, PyAny>, name: &str) -> Option<Bound<'py, PyAny>> {
    attr(object, name).and_then(|method| method.call0().ok())
}

pub fn call1<'py>(
    object: &Bound<'py, PyAny>,
    name: &str,
    argument: impl IntoPyObject<'py>,
) -> Option<Bound<'py, PyAny>> {
    attr(object, name).and_then(|method| method.call1((argument,)).ok())
}

pub fn list_attr<'py>(object: &Bound<'py, PyAny>, name: &str) -> Vec<Bound<'py, PyAny>> {
    attr(object, name)
        .and_then(|value| value.try_iter().ok())
        .map(|iterator| iterator.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

pub fn string_attr(object: &Bound<'_, PyAny>, name: &str) -> Option<String> {
    attr(object, name).and_then(|value| value.extract::<String>().ok())
}

pub fn bool_attr(object: &Bound<'_, PyAny>, name: &str) -> Option<bool> {
    attr(object, name).and_then(|value| value.extract::<bool>().ok())
}

pub fn number_attr(object: &Bound<'_, PyAny>, name: &str) -> Option<f64> {
    attr(object, name).and_then(|value| value.extract::<f64>().ok())
}

pub fn type_name(object: &Bound<'_, PyAny>) -> String {
    object
        .get_type()
        .getattr("__name__")
        .ok()
        .and_then(|name| name.extract::<String>().ok())
        .unwrap_or_else(|| "object".into())
}

pub fn safe_value(value: &Bound<'_, PyAny>) -> Value {
    if value.is_none() {
        return Value::Null;
    }
    if let Ok(boolean) = value.extract::<bool>() {
        return Value::Bool(boolean);
    }
    if let Ok(integer) = value.extract::<i64>() {
        return Value::Number(integer.into());
    }
    if let Ok(number) = value.extract::<f64>() {
        return serde_json::Number::from_f64(number)
            .map(Value::Number)
            .unwrap_or(Value::Null);
    }
    if let Ok(string) = value.extract::<String>() {
        return Value::String(string);
    }
    if let Ok(dictionary) = value.cast::<PyDict>() {
        let mut result = Map::new();
        for (key, item) in dictionary.iter() {
            let name = key
                .extract::<String>()
                .unwrap_or_else(|_| key.str().map(|s| s.to_string()).unwrap_or_default());
            result.insert(name, safe_value(&item));
        }
        return Value::Object(result);
    }
    if let Ok(list) = value.cast::<PyList>() {
        return Value::Array(list.iter().map(|item| safe_value(&item)).collect());
    }
    if let Ok(tuple) = value.cast::<PyTuple>() {
        return Value::Array(tuple.iter().map(|item| safe_value(&item)).collect());
    }
    if let Some(vector) = vector(value) {
        return vector;
    }
    for attribute_name in ["Value", "UserString"] {
        if let Some(attribute) = attr(value, attribute_name) {
            return serde_json::json!({"value": safe_value(&attribute), "type": type_name(value)});
        }
    }
    let representation = value
        .repr()
        .map(|representation| representation.to_string())
        .unwrap_or_default();
    serde_json::json!({"type": type_name(value), "repr": representation.chars().take(256).collect::<String>()})
}

pub fn vector(value: &Bound<'_, PyAny>) -> Option<Value> {
    let lower = (
        number_attr(value, "x"),
        number_attr(value, "y"),
        number_attr(value, "z"),
    );
    if let (Some(x), Some(y), Some(z)) = lower {
        return Some(serde_json::json!({"x": x, "y": y, "z": z}));
    }
    let upper = (
        number_attr(value, "X"),
        number_attr(value, "Y"),
        number_attr(value, "Z"),
    );
    if let (Some(x), Some(y), Some(z)) = upper {
        return Some(serde_json::json!({"x": x, "y": y, "z": z}));
    }
    None
}

pub fn py_value<'py>(py: Python<'py>, value: &Value) -> PyResult<Py<PyAny>> {
    match value {
        Value::Null => Ok(py.None()),
        Value::Bool(value) => Ok(value.into_pyobject(py)?.to_owned().unbind().into_any()),
        Value::Number(value) if value.is_i64() => Ok(value
            .as_i64()
            .unwrap()
            .into_pyobject(py)?
            .unbind()
            .into_any()),
        Value::Number(value) => Ok(value
            .as_f64()
            .unwrap()
            .into_pyobject(py)?
            .unbind()
            .into_any()),
        Value::String(value) => Ok(PyString::new(py, value).unbind().into_any()),
        Value::Array(values) => {
            let values = values
                .iter()
                .map(|value| py_value(py, value))
                .collect::<PyResult<Vec<_>>>()?;
            Ok(PyList::new(py, values)?.unbind().into_any())
        }
        Value::Object(values) => {
            let dictionary = PyDict::new(py);
            for (key, value) in values {
                dictionary.set_item(key, py_value(py, value)?)?;
            }
            Ok(dictionary.unbind().into_any())
        }
    }
}

pub fn version_string(value: &Bound<'_, PyAny>) -> String {
    if let Ok(parts) = value.extract::<Vec<String>>() {
        return parts.join(".");
    }
    value
        .extract::<String>()
        .unwrap_or_else(|_| "unknown".into())
}

pub fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| {
            if byte.is_ascii_alphanumeric() || b"._-~".contains(&byte) {
                vec![byte as char]
            } else {
                format!("%{byte:02X}").chars().collect()
            }
        })
        .collect()
}

pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                result.push(byte);
                index += 3;
                continue;
            }
        }
        result.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}
