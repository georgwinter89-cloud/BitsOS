//! Typed arguments of a call. The kernel has already checked them against the registered schema (types,
//! required members); these helpers turn them into Rust values and give the caller's mistakes as
//! `Reply::BadArgument` (JSON-RPC -32602), never a panic.

use mcpreg::{Json, Reply};

pub struct Args<'a>(pub &'a Json);

impl<'a> Args<'a> {
    pub fn get(&self, name: &str) -> Option<&'a Json> {
        self.0.get(name).filter(|v| !matches!(v, Json::Null))
    }

    pub fn str(&self, name: &str) -> Result<&'a str, Reply> {
        self.opt_str(name)?.ok_or_else(|| Reply::bad_argument(format!("{name}: missing")))
    }

    pub fn opt_str(&self, name: &str) -> Result<Option<&'a str>, Reply> {
        match self.get(name) {
            None => Ok(None),
            Some(v) => v.as_str().map(Some).ok_or_else(|| Reply::bad_argument(format!("{name}: want a string"))),
        }
    }

    pub fn bool_or(&self, name: &str, default: bool) -> Result<bool, Reply> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => v.as_bool().ok_or_else(|| Reply::bad_argument(format!("{name}: want true or false"))),
        }
    }

    /// An integer in `lo..=hi`, `default` when absent.
    pub fn u64_in(&self, name: &str, default: u64, lo: u64, hi: u64) -> Result<u64, Reply> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => match v.as_u64() {
                Some(n) if (lo..=hi).contains(&n) => Ok(n),
                _ => Err(Reply::bad_argument(format!("{name}: want an integer from {lo} to {hi}"))),
            },
        }
    }

    /// An object's members in their order.
    pub fn members(&self, name: &str) -> Result<Option<&'a [(String, Json)]>, Reply> {
        match self.get(name) {
            None => Ok(None),
            Some(Json::Obj(m)) => Ok(Some(m)),
            Some(_) => Err(Reply::bad_argument(format!("{name}: want an object"))),
        }
    }
}
