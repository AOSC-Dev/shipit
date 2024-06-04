mod bot;
mod db;

use std::{borrow::Cow, sync::Arc};

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use bot::{answer, Command};
use db::{Build, Db};
use eyre::Result;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use snafu::{ensure, ResultExt, Snafu};
use teloxide::{
    dispatching::{Dispatcher, HandlerExt, UpdateFilterExt},
    dptree,
    requests::Requester,
    types::{ChatId, Message, Update},
    Bot,
};
use tokio::sync::Mutex;
use tracing::{info, level_filters::LevelFilter};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

struct AppState {
    bot: Bot,
    db: Mutex<Db>,
    secret: String,
}

const ARCHS: &[&str] = &[
    "amd64",
    "arm64",
    "loongarch64",
    "ppc64el",
    "loongson3",
    "riscv64",
];

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let env_log = EnvFilter::try_from_default_env();

    if let Ok(filter) = env_log {
        tracing_subscriber::registry()
            .with(
                fmt::layer()
                    .event_format(
                        tracing_subscriber::fmt::format()
                            .with_file(true)
                            .with_line_number(true),
                    )
                    .with_filter(filter),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(
                fmt::layer()
                    .event_format(
                        tracing_subscriber::fmt::format()
                            .with_file(true)
                            .with_line_number(true),
                    )
                    .with_filter(LevelFilter::INFO),
            )
            .init();
    }

    let listen = std::env::var("shipit")?;
    let db_uri = std::env::var("shipit_redis")?;
    let secret = std::env::var("shipit_secret")?;
    let db = Mutex::new(Db::new(&db_uri).await?);

    let bot = Bot::from_env();

    let ac = Arc::new(AppState {
        bot: bot.clone(),
        db,
        secret,
    });

    let handler =
        Update::filter_message().branch(dptree::entry().filter_command::<Command>().endpoint(
            |bot: Bot, msg: Message, cmd: Command, state: Arc<AppState>| async move {
                answer(bot, msg, cmd, state).await
            },
        ));

    let mut telegram = Dispatcher::builder(bot, handler)
        // // Pass the shared state to the handler as a dependency.
        .dependencies(dptree::deps![ac.clone()])
        .enable_ctrlc_handler()
        .build();

    tokio::spawn(async move { telegram.dispatch().await });

    info!("shipit running at: {}", listen);
    let app = Router::new()
        .route("/done", post(build_done))
        .route("/workerisstarted", get(build_is_started))
        .with_state(ac);
    let listener = tokio::net::TcpListener::bind(listen).await.unwrap();
    axum::serve(listener, app).await?;

    Ok(())
}

#[derive(Deserialize)]
struct BuildDoneRequest {
    id: i64,
    arch: String,
    build_type: BuildTypeRequest,
    has_error: bool,
    log_url: Option<String>,
    push_success: bool,
    date: String
}

#[derive(Deserialize)]
struct BuildTypeRequest {
    name: String,
    variants: Option<Vec<String>>,
}

#[derive(Debug, Snafu)]
enum BuildRequestError {
    #[snafu(display("Failed to mod redis database."))]
    Redis { source: eyre::Error },
    #[snafu(display("Bad secret."))]
    BadSecret,
    #[snafu(transparent)]
    Teloxide {
        source: teloxide::errors::RequestError,
    },
}

impl IntoResponse for BuildRequestError {
    fn into_response(self) -> axum::response::Response {
        match self {
            BuildRequestError::Redis { ref source } => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("{}: {}", self, source),
            )
                .into_response(),
            BuildRequestError::BadSecret => {
                (StatusCode::BAD_REQUEST, self.to_string()).into_response()
            }
            BuildRequestError::Teloxide { .. } => {
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()).into_response()
            }
        }
    }
}

async fn build_done(
    header: HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(request): Json<BuildDoneRequest>,
) -> Result<(), BuildRequestError> {
    let AppState { bot, db, secret } = &*state;

    ensure!(
        header.get("secret").map(|x| *x == secret).unwrap_or(false),
        BadSecretSnafu
    );

    let mut db = db.lock().await;
    db.set_build_done(&request.arch).await.context(RedisSnafu)?;

    bot.send_message(
        ChatId(request.id),
        format!(
            "Build {}{} {}: {}\nlog url: {}\nPush success: {}",
            request.build_type.name,
            if let Some(v) = request.build_type.variants {
                Cow::Owned(format!(" ({})", v.join(" ")))
            } else {
                Cow::Borrowed("")
            },
            if !request.has_error {
                "success"
            } else {
                "has error"
            },
            request.arch,
            if let Some(url) = request.log_url {
                Cow::Owned(url)
            } else {
                Cow::Borrowed("Failed to push log")
            },
            request.push_success
        ),
    )
    .await?;

    Ok(())
}

#[derive(Deserialize)]
struct BuildStartRequest {
    arch: String,
}

#[derive(Serialize)]
enum Status {
    Working(Build),
    Pending,
}

async fn build_is_started(
    header: HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(request): Query<BuildStartRequest>,
) -> Result<Json<Status>, BuildRequestError> {
    let AppState { db, secret, .. } = &*state;

    ensure!(
        header.get("secret").map(|x| *x == secret).unwrap_or(false),
        BadSecretSnafu
    );

    let mut db = db.lock().await;
    let build = db.get(&request.arch).await;

    match build {
        Ok(b) => Ok(Json(Status::Working(b))),
        Err(_) => Ok(Json(Status::Pending)),
    }
}
