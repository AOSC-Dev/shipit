use std::{borrow::Cow, sync::Arc};

use teloxide::{
    requests::{Requester, ResponseResult},
    types::{ChatId, Message},
    utils::command::BotCommands,
    Bot,
};

use tracing::error;

use crate::{
    db::{Build, BuildType},
    AppState, ARCHS,
};

#[derive(BotCommands, Clone, Debug)]
#[command(
    rename_rule = "lowercase",
    description = "ReleaseIt! supports the following commands:"
)]
pub enum Command {
    #[command(description = "Display usage: /help")]
    Help,
    #[command(description = "start")]
    Start(String),
    #[command(description = "Login")]
    Login,
    #[command(
        description = "Start a build job: /build [type] [archs] (e.g., /build livekit amd64,arm64)"
    )]
    Build(String),
    #[command(description = "Show queue and server status: /status")]
    Status,
}

pub async fn answer(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    let AppState { db, secret, .. } = &*state;

    match cmd {
        Command::Help => {
            bot.send_message(msg.chat.id, Command::descriptions().to_string())
                .await?;
        }
        Command::Build(args) => {
            let is_login = is_login(&msg.chat.id, secret).await;

            if !is_login {
                return Ok(());
            }

            let mut db = db.lock().await;
            let mut args_split = args.split_ascii_whitespace();

            let build_type = args_split.next();

            let build_type = match build_type {
                Some("livekit") => BuildType::Livekit,
                Some("release") => BuildType::Release,
                _ => {
                    bot.send_message(msg.chat.id, Command::descriptions().to_string())
                        .await?;
                    return Ok(());
                }
            };

            let archs = args_split.next();

            let archs = match archs {
                Some(a) => a.split_whitespace().collect::<Vec<_>>(),
                None => ARCHS.iter().map(|x| x.to_owned()).collect::<Vec<_>>(),
            };

            for i in archs {
                if db.get(i).await.is_ok() {
                    bot.send_message(msg.chat.id, "Another build task is already running.")
                        .await?;
                    return Ok(());
                }

                match db
                    .set_building(
                        i,
                        &Build {
                            id: msg.chat.id.0,
                            arch: i.to_string(),
                            build_type: build_type.clone(),
                        },
                    )
                    .await
                {
                    Ok(_) => {
                        bot.send_message(msg.chat.id, format!("Building {} for {}", build_type, i))
                            .await?;
                    }
                    Err(e) => {
                        bot.send_message(
                            msg.chat.id,
                            format!("Failed to mod redis database: {}", e),
                        )
                        .await?;
                    }
                }
            }
        }
        Command::Status => {
            let mut db = db.lock().await;
            let map = db.running_worker().await;
            let mut res = String::new();

            match map {
                Ok(m) => {
                    for b in m {
                        res.push_str(&format!("{}: building {}\n", b.arch, b.build_type));
                    }

                    bot.send_message(msg.chat.id, res).await?;
                }
                Err(e) => {
                    bot.send_message(
                        msg.chat.id,
                        truncate(&format!("Failed to mod redis database: {}", e)),
                    )
                    .await?;
                }
            }
        }
        Command::Login => {
            bot.send_message(msg.chat.id, "https://github.com/login/oauth/authorize?client_id=Iv1.bf26f3e9dd7883ae&redirect_uri=https://minzhengbu.aosc.io/login").await?;
        }
        Command::Start(arguments) => {
            if arguments.len() != 20 {
                bot.send_message(msg.chat.id, Command::descriptions().to_string())
                    .await?;
                return Ok(());
            } else {
                let resp = login_github(&msg, arguments).await;

                match resp {
                    Ok(_) => bot.send_message(msg.chat.id, "Login successful!").await?,
                    Err(e) => {
                        bot.send_message(
                            msg.chat.id,
                            truncate(&format!("Login failed with error: {e}")),
                        )
                        .await?
                    }
                };
            }
        }
    }

    Ok(())
}

pub async fn login_github(
    msg: &Message,
    arguments: String,
) -> Result<reqwest::Response, reqwest::Error> {
    let client = reqwest::Client::new();

    client
        .get("https://minzhengbu.aosc.io/login_from_telegram".to_string())
        .query(&[
            ("telegram_id", msg.chat.id.0.to_string()),
            ("rid", arguments),
        ])
        .send()
        .await
        .and_then(|x| x.error_for_status())
}

fn truncate<'a>(text: &str) -> Cow<str> {
    if text.chars().count() > 1000 {
        console::truncate_str(text, 1000, "...")
    } else {
        Cow::Borrowed(text)
    }
}

pub async fn is_login(msg_chatid: &ChatId, secret: &str) -> bool {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://minzhengbu.aosc.io/get_token")
        .query(&[("id", &msg_chatid.0.to_string())])
        .header("secret", secret)
        .send()
        .await
        .and_then(|r| r.error_for_status());

    match resp {
        Ok(_) => true,
        Err(e) => {
            error!("{e}");
            false
        }
    }
}
