use anyhow::Result;
use std::ops::{Deref, DerefMut};

use postgres_types::FromSqlOwned;
use tokio_postgres::Row;


#[derive(Debug, Clone)]
pub struct WithId<T, Id = i32> {
    pub inner: T,
    pub id: Id,
}

impl<T, Id> Deref for WithId<T, Id> {
    type Target = T;

    fn deref(&self) -> &Self::Target
    {
        &self.inner
    }
}

impl<T, Id> DerefMut for WithId<T, Id> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<'a, T, Id> TryFrom<&'a Row> for WithId<T, Id>
    where
        T: TryFrom<&'a Row, Error = anyhow::Error>,
        Id: FromSqlOwned,
{
    type Error = anyhow::Error;

    fn try_from(row: &'a Row) -> Result<Self> {
        let id: Id = row.try_get("id")?;
        let inner = T::try_from(row)?;
        Ok(WithId { inner, id })
    }
}


// pub struct WithTimestamps<T> {
//     pub inner: T,
//     pub created_at: (),
//     pub updated_at: (),
// }
