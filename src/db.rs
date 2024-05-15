use std::collections::HashMap;

use redis::{aio::MultiplexedConnection, AsyncCommands, RedisResult};

use crate::ARCHS;

pub struct Db {
    conn: MultiplexedConnection,
}

impl Db {
    pub async fn new(redis: &str) -> RedisResult<Self> {
        let client = redis::Client::open(redis)?;
        let mut conn = client.get_multiplexed_tokio_connection().await?;

        for i in ARCHS {
            if conn.get::<_, i64>(format!("shipit:{i}")).await.is_err() {
                conn.set(format!("shipit:{i}"), -1).await?;
            }
        }

        Ok(Self { conn })
    }

    pub async fn get(&mut self, arch: &str) -> RedisResult<i64> {
        Ok(self.conn.get::<_, _>(format!("shipit:{arch}")).await?)
    }

    pub async fn set_building(&mut self, arch: &str, id: i64) -> RedisResult<()> {
        self.conn.set(format!("shipit:{arch}"), id).await?;

        Ok(())
    }

    pub async fn set_build_done(&mut self, arch: &str) -> RedisResult<()> {
        self.conn.set(format!("shipit:{arch}"), -1).await?;

        Ok(())
    }

    pub async fn all_worker(&mut self) -> RedisResult<HashMap<String, bool>> {
        let s: Vec<String> = redis::cmd("KEYS")
            .arg(format!("shipit:*"))
            .query_async(&mut self.conn)
            .await?;

        let prefix = "shipit:";

        let mut map = HashMap::new();

        for i in s {
            let id = self.get(&i).await?;
            if id == -1 {
                map.insert(i.strip_prefix(prefix).unwrap().to_string(), false);
            } else {
                map.insert(i.strip_prefix(prefix).unwrap().to_string(), true);
            }
        }

        Ok(map)
    }

    pub async fn worker_is_start(&mut self, arch: &str) -> RedisResult<i64> {
        let id = self.get(&arch).await?;

        Ok(id)
    }
}
