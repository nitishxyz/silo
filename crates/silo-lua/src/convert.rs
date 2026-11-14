//! JSON ⇄ Lua. JSON `null` in an object is a missing key (so `if ev.cwd`
//! works); in an array it is `silo.json.null` (mlua's null), to keep
//! positions. Arrays carry mlua's array metatable, so an empty one goes
//! back out as `[]`; any other empty table is `{}`.

use mlua::{Lua, LuaSerdeExt, Table, Value as LuaValue};
use serde_json::{Map, Number, Value};

/// Deepest table nesting converted; deeper is taken for a cycle.
const MAX_DEPTH: usize = 128;

/// `value` as a Lua value.
///
/// # Errors
/// Lua out of memory.
pub fn to_lua(lua: &Lua, value: &Value) -> mlua::Result<LuaValue> {
    Ok(match value {
        Value::Null => LuaValue::Nil,
        Value::Bool(b) => LuaValue::Boolean(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => LuaValue::Integer(i),
            None => LuaValue::Number(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => LuaValue::String(lua.create_string(s)?),
        Value::Array(items) => {
            let table = lua.create_table_with_capacity(items.len(), 0)?;
            for (i, item) in items.iter().enumerate() {
                let item = if item.is_null() { lua.null() } else { to_lua(lua, item)? };
                table.raw_set(i + 1, item)?;
            }
            table.set_metatable(Some(lua.array_metatable()))?;
            LuaValue::Table(table)
        }
        Value::Object(map) => {
            let table = lua.create_table_with_capacity(0, map.len())?;
            for (key, item) in map {
                if !item.is_null() {
                    table.raw_set(key.as_str(), to_lua(lua, item)?)?;
                }
            }
            LuaValue::Table(table)
        }
    })
}

/// `value` as JSON.
///
/// # Errors
/// A function, coroutine or userdata, a NaN or infinity, a key that is
/// neither a string nor a number, or nesting deeper than 128 (a cycle).
pub fn from_lua(lua: &Lua, value: &LuaValue) -> Result<Value, String> {
    convert(lua, value, 0)
}

fn convert(lua: &Lua, value: &LuaValue, depth: usize) -> Result<Value, String> {
    if depth > MAX_DEPTH {
        return Err("a table nested too deeply (a cycle?)".into());
    }
    Ok(match value {
        LuaValue::Nil => Value::Null,
        LuaValue::Boolean(b) => Value::Bool(*b),
        LuaValue::Integer(i) => Value::from(*i),
        LuaValue::Number(n) => number(*n)?,
        LuaValue::String(s) => Value::String(s.to_string_lossy()),
        LuaValue::Table(table) => table_json(lua, table, depth)?,
        LuaValue::LightUserData(_) if *value == lua.null() => Value::Null,
        other => return Err(format!("a {} is not JSON", other.type_name())),
    })
}

/// A whole float as an integer (`3.0` is `3`), else a JSON number.
fn number(n: f64) -> Result<Value, String> {
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        #[allow(clippy::cast_possible_truncation)]
        return Ok(Value::from(n as i64));
    }
    Number::from_f64(n).map(Value::Number).ok_or_else(|| format!("{n} is not a JSON number"))
}

fn table_json(lua: &Lua, table: &Table, depth: usize) -> Result<Value, String> {
    let is_array_mt = table.metatable().is_some_and(|mt| mt == lua.array_metatable());
    let len = table.raw_len();
    let mut count = 0usize;
    let mut pairs = Vec::new();
    for pair in table.pairs::<LuaValue, LuaValue>() {
        let (key, item) = pair.map_err(|e| e.to_string())?;
        count += 1;
        pairs.push((key, item));
    }
    if is_array_mt || (len > 0 && count == len) {
        let mut items = Vec::with_capacity(len);
        for i in 1..=len {
            let item: LuaValue = table.raw_get(i).map_err(|e| e.to_string())?;
            items.push(convert(lua, &item, depth + 1)?);
        }
        return Ok(Value::Array(items));
    }
    let mut map = Map::new();
    for (key, item) in pairs {
        let key = match key {
            LuaValue::String(s) => s.to_string_lossy(),
            LuaValue::Integer(i) => i.to_string(),
            LuaValue::Number(n) => n.to_string(),
            other => return Err(format!("a {} key is not JSON", other.type_name())),
        };
        map.insert(key, convert(lua, &item, depth + 1)?);
    }
    Ok(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn values_round_trip() {
        let lua = Lua::new();
        for value in [json!({"a": 1, "b": [1, 2.5, "x", true], "c": {}, "d": []}), json!([1, null, 3]), json!("text"), json!(-4)] {
            let back = from_lua(&lua, &to_lua(&lua, &value).unwrap()).unwrap();
            assert_eq!(back, value);
        }
        // An object's nulls are missing keys.
        let back = from_lua(&lua, &to_lua(&lua, &json!({"a": null, "b": 1})).unwrap()).unwrap();
        assert_eq!(back, json!({"b": 1}));
    }

    #[test]
    fn lua_tables_become_arrays_or_objects() {
        let lua = Lua::new();
        let eval = |code: &str| from_lua(&lua, &lua.load(code).eval::<LuaValue>().unwrap());
        assert_eq!(eval("return {1, 2, 3}").unwrap(), json!([1, 2, 3]));
        assert_eq!(eval("return {x = 1, [2] = 'b'}").unwrap(), json!({"x": 1, "2": "b"}));
        assert_eq!(eval("return {}").unwrap(), json!({}));
        assert_eq!(eval("return 2.0").unwrap(), json!(2));
        assert!(eval("return function() end").is_err());
        assert!(eval("local t = {} t.t = t return t").is_err());
        assert!(eval("return 0/0").is_err());
    }
}
