use anyhow::Result;

pub trait IntoResult<T> {
    fn into_result(self) -> Result<T>;
}

impl IntoResult<()> for () {
    fn into_result(self) -> Result<()> {
        Ok(self)
    }
}

impl<T> IntoResult<T> for Result<T> {
    fn into_result(self) -> Result<T> {
        self
    }
}
