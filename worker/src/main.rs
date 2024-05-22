use std::{env::current_dir, fmt::Display, path::Path, process::Output, time::Duration};

use chrono::Local;
use eyre::OptionExt;
use reqwest::{Client, ClientBuilder};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{self, read_dir},
    process::Command,
    time::{sleep, Instant},
};
use tracing::{error, info, level_filters::LevelFilter, warn};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

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
            BuildType::Release(_) => write!(f, "release"),
        }
    }
}

#[derive(Deserialize)]
enum Status {
    Working(Build),
    Pending,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
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

    dotenvy::dotenv().ok();
    let arch = libaosc::arch::get_arch_name().ok_or_eyre("Unsupport arch")?;
    let client = ClientBuilder::new().user_agent("shipit_worker").build()?;
    let server_uri = std::env::var("shipit_uri")?;
    let secret = std::env::var("shipit_secret")?;
    let ssh_key = std::env::var("upload_ssh_key")?;
    let host = std::env::var("rsync_host")?;

    loop {
        if let Err(e) = worker(&client, &server_uri, &secret, arch, &ssh_key, &host).await {
            error!("{e}");
        }

        sleep(Duration::from_millis(300)).await;
    }
}

#[derive(Serialize)]
struct DoneRequest {
    id: i64,
    arch: String,
    build_type: BuildTypeRequest,
    has_error: bool,
    log_url: Option<String>,
    push_success: bool,
}

#[derive(Serialize)]
struct BuildTypeRequest {
    name: String,
    variants: Option<Vec<String>>,
}

impl From<BuildType> for BuildTypeRequest {
    fn from(value: BuildType) -> Self {
        match value {
            BuildType::Livekit => BuildTypeRequest {
                name: "livekit".to_owned(),
                variants: None,
            },
            BuildType::Release(v) => BuildTypeRequest {
                name: "release".to_owned(),
                variants: Some(v),
            },
        }
    }
}

async fn worker(
    client: &Client,
    uri: &str,
    secret: &str,
    arch: &str,
    upload_ssh_key: &str,
    host: &str,
) -> eyre::Result<()> {
    let resp = client
        .get(format!("{}/workerisstarted", uri))
        .header("secret", secret)
        .query(&[("arch", arch)])
        .send()
        .await?;

    let resp = resp.error_for_status()?;
    let status = resp.json::<Status>().await?;

    if let Status::Working(build) = status {
        info!("{} is started", arch);
        let (logs, success, push_success) = match build.build_type {
            BuildType::Livekit => build_livekit(host, upload_ssh_key).await?,
            BuildType::Release(ref variants) => {
                build_release(arch, variants, host, upload_ssh_key).await?
            }
        };

        let file_name = format!(
            "shipit-{}-{}-{}.txt",
            arch,
            gethostname::gethostname().to_string_lossy(),
            Local::now().format("%Y-%m-%d-%H:%M:%S")
        );

        fs::write(&file_name, logs).await?;

        let mut log_url = None;
        let mut scp_log = vec![];
        if run_logged_with_retry(
            "scp",
            &[
                "-i",
                &upload_ssh_key,
                "./log",
                &format!("maintainers@{}:/buildit/logs", host),
            ],
            Path::new("."),
            &mut scp_log,
        )
        .await?
        {
            fs::remove_file("./log").await?;
            log_url = Some(format!("https://buildit.aosc.io/logs/{file_name}"));
        } else {
            error!(
                "Failed to scp log to repo: {}",
                String::from_utf8_lossy(&scp_log)
            );
        };

        if log_url.is_none() {
            let dir = Path::new("./push_failed_logs");
            let to = dir.join(&file_name);
            fs::create_dir_all(dir).await?;
            fs::copy(file_name, to).await?;
        }

        let resp = client
            .post(format!("{uri}/done"))
            .header("secret", secret)
            .json(&DoneRequest {
                id: build.id,
                arch: build.arch,
                build_type: BuildTypeRequest::from(build.build_type),
                has_error: !success,
                push_success,
                log_url,
            })
            .send()
            .await?;

        resp.error_for_status()?;
    }

    Ok(())
}

async fn build_livekit(host: &str, upload_ssh_key: &str) -> eyre::Result<(Vec<u8>, bool, bool)> {
    let mklive_dir = Path::new("aosc-mklive");
    let mut logs = vec![];
    if !mklive_dir.is_dir() {
        get_output_logged(
            "git",
            &["clone", "https://github.com/AOSC-Dev/aosc-mklive"],
            Path::new("."),
            &mut logs,
        )
        .await?;
    }
    get_output_logged("git", &["pull"], mklive_dir, &mut logs).await?;
    let mut dir = read_dir(mklive_dir).await?;
    loop {
        if let Ok(Some(i)) = dir.next_entry().await {
            let path = i.path();

            if path
                .extension()
                .map(|x| x == "iso" || x == "sha256sum")
                .unwrap_or(false)
            {
                fs::remove_file(i.path()).await?;
            }

            let name = path.file_name();
            if name
                .map(|x| {
                    ["livekit", "iso", "to-squash", "memtest", "sb"]
                        .contains(&x.to_string_lossy().to_string().as_str())
                })
                .unwrap_or(false)
            {
                fs::remove_dir_all(i.path()).await?;
            }
        } else {
            break;
        }
    }
    let mklive = get_output_logged("bash", &["./aosc-mklive.sh"], mklive_dir, &mut logs).await?;
    let success = mklive.status.success();

    let mut push_success = true;

    let mut dir = read_dir(mklive_dir).await?;
    loop {
        if let Ok(Some(i)) = dir.next_entry().await {
            if i.path()
                .extension()
                .map(|x| x == "iso" || x == "sha256sum")
                .unwrap_or(false)
            {
                push_success = run_logged_with_retry(
                    "scp",
                    &[
                        "-i",
                        upload_ssh_key,
                        "-r",
                        &i.path().canonicalize()?.to_string_lossy(),
                        &format!("maintainers@{}:/lookaside/private/aosc-os/", host),
                    ],
                    current_dir()?.as_path(),
                    &mut logs,
                )
                .await
                .unwrap_or(false);
            }
        } else {
            break;
        }
    }

    Ok((logs, success, push_success))
}

async fn get_output_logged(
    cmd: &str,
    args: &[&str],
    cwd: &Path,
    logs: &mut Vec<u8>,
) -> eyre::Result<Output> {
    let begin = Instant::now();
    let msg = format!(
        "{}: Running `{} {}` in `{}`\n",
        Local::now(),
        cmd,
        args.join(" "),
        cwd.display()
    );
    logs.extend(msg.as_bytes());
    info!("{}", msg.trim());

    let output = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .await?;

    let elapsed = begin.elapsed();
    logs.extend(
        format!(
            "{}: `{} {}` finished in {:?} with {}\n",
            Local::now(),
            cmd,
            args.join(" "),
            elapsed,
            output.status
        )
        .as_bytes(),
    );
    logs.extend("STDOUT:\n".as_bytes());
    logs.extend(output.stdout.clone());
    logs.extend("STDERR:\n".as_bytes());
    logs.extend(output.stderr.clone());

    Ok(output)
}

async fn run_logged_with_retry(
    cmd: &str,
    args: &[&str],
    cwd: &Path,
    logs: &mut Vec<u8>,
) -> eyre::Result<bool> {
    for i in 0..5 {
        if i > 0 {
            info!("Attempt #{i} to run `{cmd} {}`", args.join(" "));
        }
        match get_output_logged(cmd, args, cwd, logs).await {
            Ok(output) => {
                if output.status.success() {
                    return Ok(true);
                } else {
                    warn!(
                        "Running `{cmd} {}` exited with {}",
                        args.join(" "),
                        output.status
                    );
                }
            }
            Err(err) => {
                warn!("Running `{cmd} {}` failed with {err}", args.join(" "));
            }
        }
        // exponential backoff
        sleep(Duration::from_secs(1 << i)).await;
    }
    warn!("Failed too many times running `{cmd} {}`", args.join(" "));

    Ok(false)
}

async fn build_release(
    arch: &str,
    variants: &[String],
    host: &str,
    upload_ssh_key: &str,
) -> eyre::Result<(Vec<u8>, bool, bool)> {
    let aoscbootstrap_dir = Path::new("aoscbootstrap");
    let mut logs = vec![];
    if !aoscbootstrap_dir.is_dir() {
        get_output_logged(
            "git",
            &["clone", "https://github.com/AOSC-Dev/aoscbootstrap"],
            Path::new("."),
            &mut logs,
        )
        .await?;
    }

    get_output_logged("git", &["pull"], aoscbootstrap_dir, &mut logs).await?;

    let os_dir_str = format!("os-{}", arch);
    let os_dir = aoscbootstrap_dir.join(&os_dir_str);

    if os_dir.exists() {
        info!("{os_dir_str} exists, removing ...");
        fs::remove_dir_all(&os_dir).await?;
    }

    let mut args = vec!["./contrib/generate-releases.sh"];

    args.extend(variants.iter().map(|x| x.as_str()));

    let general_release = get_output_logged("bash", &args, aoscbootstrap_dir, &mut logs).await?;
    let success = general_release.status.success();

    let scp_image = run_logged_with_retry(
        "scp",
        &[
            "-i",
            upload_ssh_key,
            "-r",
            &os_dir_str,
            &format!("maintainers@{}:/lookaside/private/aosc-os", host),
        ],
        &aoscbootstrap_dir,
        &mut logs,
    )
    .await
    .unwrap_or(false);

    Ok((logs, success, scp_image))
}
