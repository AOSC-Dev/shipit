use std::fmt::Display;

use redis::{aio::MultiplexedConnection, AsyncCommands};
use serde::{Deserialize, Serialize};

pub struct Db {
    conn: MultiplexedConnection,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Build {
    pub id: i64,
    pub arch: String,
    pub build_type: BuildType,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum BuildType {
    Livekit,
    Release(Vec<String>),
}

impl Display for BuildType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildType::Livekit => write!(f, "livekit"),
            BuildType::Release(v) => write!(f, "release variant: {}", v.join(" ")),
        }
    }
}

impl Db {
    pub async fn new(redis: &str) -> eyre::Result<Self> {
        let client = redis::Client::open(redis)?;
        let conn = client.get_multiplexed_tokio_connection().await?;

        Ok(Self { conn })
    }

    pub async fn get(&mut self, arch: &str) -> eyre::Result<Build> {
        let s: String = self.conn.get::<_, _>(format!("shipit:{arch}")).await?;

        Ok(serde_json::from_str(&s)?)
    }

    pub async fn set_building(&mut self, arch: &str, build: &Build) -> eyre::Result<()> {
        self.conn
            .set(format!("shipit:{arch}"), serde_json::to_string(build)?)
            .await?;

        Ok(())
    }

    pub async fn set_build_done(&mut self, arch: &str) -> eyre::Result<()> {
        self.conn.del(format!("shipit:{arch}")).await?;

        Ok(())
    }

    pub async fn running_worker(&mut self) -> eyre::Result<Vec<Build>> {
        let s: Vec<String> = redis::cmd("KEYS")
            .arg("shipit:*".to_string())
            .query_async(&mut self.conn)
            .await?;

        let mut v = vec![];
        for i in s {
            let entry = self.conn.get::<_, String>(i).await?;
            v.push(serde_json::from_str(&entry)?);
        }

        Ok(v)
    }
}
