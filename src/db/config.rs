use anyhow::Result;
use tokio_postgres::GenericClient;

use crate::db::{property, get_property, set_property};


pub enum Parameter {
    Domain,
}


pub async fn get_domain(db: &impl GenericClient) -> Result<Option<String>>
{
    get_property(db, property::DOMAIN).await
}

pub async fn get_domain_or_default(db: &impl GenericClient) -> Result<String>
{
    get_domain(db).await.map(|o| o.unwrap_or_else(|| "scree.example.com".to_string()))
}

pub async fn set_domain(db: &impl GenericClient, value: &str) -> Result<()>
{
    set_property(db, property::DOMAIN, Some(value)).await
}
