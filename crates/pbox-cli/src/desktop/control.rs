//! One-shot computer controls over the existing authenticated desktop tunnel.
use super::*;
use tokio::net::TcpStream;

#[derive(Debug, Args)]
pub(super) struct Target {
    /// Box ID or name.
    id: String,
    /// Registered desktop session.
    #[arg(long)]
    session: Option<String>,
    #[arg(long)]
    endpoint: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum Button {
    Left,
    Middle,
    Right,
}
impl Button {
    fn mask(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Middle => 2,
            Self::Right => 4,
        }
    }
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum Direction {
    Up,
    Down,
    Left,
    Right,
}
impl Direction {
    fn mask(self) -> u8 {
        match self {
            Self::Up => 8,
            Self::Down => 16,
            Self::Left => 32,
            Self::Right => 64,
        }
    }
}

#[derive(Debug, Args)]
pub(super) struct Point {
    /// Horizontal pixel coordinate, starting at zero on the left.
    x: u16,
    /// Vertical pixel coordinate, starting at zero at the top.
    y: u16,
}

#[derive(Debug, Subcommand)]
pub(super) enum Action {
    /// Save the desktop as PNG. JSON without --output includes base64 PNG.
    #[command(visible_alias = "read")]
    Screenshot {
        #[command(flatten)]
        target: Target,
        /// Local PNG path (default: desktop.png). Use - for raw PNG stdout.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Type literal text, including Unicode, without a local TTY.
    Type {
        #[command(flatten)]
        target: Target,
        #[arg(required_unless_present = "stdin", conflicts_with = "stdin")]
        text: Option<String>,
        /// Read UTF-8 text from stdin (maximum 64 KiB).
        #[arg(long)]
        stdin: bool,
    },
    /// Send key chords in order, e.g. --key Ctrl+L --key Enter.
    Send {
        #[command(flatten)]
        target: Target,
        #[arg(long = "key", required = true)]
        keys: Vec<String>,
    },
    /// Move the mouse to an absolute pixel position.
    Move {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        point: Point,
    },
    /// Click at an absolute pixel position.
    Click {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        point: Point,
        #[arg(long, value_enum, default_value = "left")]
        button: Button,
        /// Number of clicks (use 2 for double-click).
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=3))]
        count: u8,
    },
    /// Hold a mouse button while moving between two pixel positions.
    Drag {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        point: Point,
        to_x: u16,
        to_y: u16,
        #[arg(long, value_enum, default_value = "left")]
        button: Button,
        #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(20..=5000))]
        duration_ms: u64,
    },
    /// Scroll at a pixel position by a number of wheel steps.
    Scroll {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        point: Point,
        #[arg(value_enum)]
        direction: Direction,
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u16).range(1..=100))]
        steps: u16,
    },
}

impl Action {
    fn target(&self) -> &Target {
        match self {
            Self::Screenshot { target, .. }
            | Self::Type { target, .. }
            | Self::Send { target, .. }
            | Self::Move { target, .. }
            | Self::Click { target, .. }
            | Self::Drag { target, .. }
            | Self::Scroll { target, .. } => target,
        }
    }
    fn name(&self) -> &'static str {
        match self {
            Self::Screenshot { .. } => "screenshot",
            Self::Type { .. } => "type",
            Self::Send { .. } => "send",
            Self::Move { .. } => "move",
            Self::Click { .. } => "click",
            Self::Drag { .. } => "drag",
            Self::Scroll { .. } => "scroll",
        }
    }
    // Validate the entire input before connecting or sending any events.
    fn prepare(&self, json: bool) -> Result<Vec<Vec<u32>>> {
        match self {
            Self::Screenshot { output, .. } => {
                anyhow::ensure!(
                    !(json && output.as_deref() == Some(Path::new("-"))),
                    "--json cannot be combined with --output -"
                );
                anyhow::ensure!(
                    output.as_deref() != Some(Path::new("-")) || !io::stdout().is_terminal(),
                    "redirect PNG stdout or specify --output FILE"
                );
                Ok(Vec::new())
            }
            Self::Type { text, stdin, .. } => {
                let text = if *stdin {
                    let mut text = String::new();
                    io::stdin()
                        .take(65_537)
                        .read_to_string(&mut text)
                        .context("read UTF-8 desktop input")?;
                    text
                } else {
                    text.clone().unwrap_or_default()
                };
                anyhow::ensure!(text.len() <= 65_536, "desktop text is limited to 64 KiB");
                anyhow::ensure!(
                    !text
                        .chars()
                        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')),
                    "use send --key for control keys"
                );
                Ok(text.chars().map(|c| vec![rfb::character(c)]).collect())
            }
            Self::Send { keys, .. } => {
                anyhow::ensure!(keys.len() <= 1024, "at most 1024 key chords per command");
                keys.iter().map(|key| rfb::chord(key)).collect()
            }
            _ => Ok(Vec::new()),
        }
    }
}

pub(super) fn run(store: &ConfigStore, action: Action, json: bool) -> Result<()> {
    let keys = action.prepare(json)?;
    let target = action.target();
    let config = load_config(store)?;
    let (id, endpoint) = resolve_agent_endpoint(&config, &target.id, target.endpoint.as_deref())?;
    let materials = agent_materials(&config, &id)?;
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let mut agent = relay::connect_agent(&config, &endpoint, &id, &materials.ca.certificate_pem, &materials.client).await?;
        let session = start_session(&mut agent, &id, target.session.clone()).await?;
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let socket = TcpStream::connect(listener.local_addr()?).await?;
        let (forwarded, _) = listener.accept().await?;
        drop(listener);
        // No detached task: cancelling or completing the operation drops both futures.
        let forward = agent.forward_tcp(forwarded, "127.0.0.1", session.port);
        let operation = async {
            let mut client = rfb::Client::connect(socket).await?;
            let pixels = execute(&mut client, &action, &keys).await?;
            Ok::<_, anyhow::Error>((client.width, client.height, pixels))
        };
        let (width, height, pixels) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::select! {
                result = operation => result,
                result = forward => { result.context("desktop tunnel failed")?; bail!("desktop tunnel closed before the operation completed") }
            }
        }).await.context("desktop control timed out; input may have been delivered, do not retry blindly")?
          .context("desktop control failed; input is never retried")?;
        report(&action, &id, &session.session, width, height, &pixels, json)
    })
}

async fn execute<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    client: &mut rfb::Client<S>,
    action: &Action,
    keys: &[Vec<u32>],
) -> Result<Vec<u8>> {
    match action {
        Action::Screenshot { .. } => {}
        Action::Type { .. } | Action::Send { .. } => {
            for chord in keys {
                client.chord(chord).await?;
            }
        }
        Action::Move { point, .. } => client.pointer(point.x, point.y, 0).await?,
        Action::Click {
            point,
            button,
            count,
            ..
        } => {
            client.pointer(point.x, point.y, 0).await?;
            for n in 0..*count {
                if n > 0 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                client.pointer(point.x, point.y, button.mask()).await?;
                client.pointer(point.x, point.y, 0).await?;
            }
        }
        Action::Drag {
            point,
            to_x,
            to_y,
            button,
            duration_ms,
            ..
        } => {
            client.check_point(*to_x, *to_y)?;
            client.pointer(point.x, point.y, 0).await?;
            client.pointer(point.x, point.y, button.mask()).await?;
            let steps = (*duration_ms / 20) as i32;
            for step in 1..=steps {
                tokio::time::sleep(Duration::from_millis(*duration_ms / steps as u64)).await;
                let x = i32::from(point.x) + (i32::from(*to_x) - i32::from(point.x)) * step / steps;
                let y = i32::from(point.y) + (i32::from(*to_y) - i32::from(point.y)) * step / steps;
                client.pointer(x as u16, y as u16, button.mask()).await?;
            }
            client.pointer(*to_x, *to_y, 0).await?;
        }
        Action::Scroll {
            point,
            direction,
            steps,
            ..
        } => {
            client.pointer(point.x, point.y, 0).await?;
            for _ in 0..*steps {
                client.pointer(point.x, point.y, direction.mask()).await?;
                client.pointer(point.x, point.y, 0).await?;
            }
        }
    }
    client.screen().await
}

fn report(
    action: &Action,
    id: &str,
    session: &str,
    width: u16,
    height: u16,
    pixels: &[u8],
    json: bool,
) -> Result<()> {
    let mut receipt = serde_json::json!({"id": id, "session": session, "action": action.name(), "width": width, "height": height});
    let mut saved = None;
    if let Action::Screenshot { output, .. } = action {
        let png = rfb::png(width, height, pixels)?;
        receipt["format"] = "png".into();
        match output.as_deref() {
            Some(path) if path == Path::new("-") => {
                io::stdout().lock().write_all(&png)?;
                return Ok(());
            }
            None if json => {
                receipt["data_base64"] = base64::engine::general_purpose::STANDARD
                    .encode(&png)
                    .into()
            }
            path => {
                let path = path.unwrap_or_else(|| Path::new("desktop.png"));
                fs::write(path, &png)
                    .with_context(|| format!("write screenshot {}", path.display()))?;
                receipt["output"] = path.to_string_lossy().as_ref().into();
                saved = Some(path.to_string_lossy().into_owned());
            }
        }
    } else {
        receipt["sent"] = true.into();
    }
    if json {
        ui::json_text(&serde_json::to_string(&receipt)?);
    } else {
        ui::desktop_control_result(action.name(), id, session, width, height, saved.as_deref());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Action {
        let cli = Cli::try_parse_from(["pbox", "desktop"].into_iter().chain(words.iter().copied()))
            .unwrap();
        match cli.command {
            Command::Desktop(command) => command.action.unwrap(),
            _ => panic!(),
        }
    }

    #[test]
    fn commands_parse_and_validate_before_connecting() {
        for words in [
            vec!["screenshot", "current"],
            vec!["read", "current", "--output", "-"],
            vec!["type", "current", "hello"],
            vec!["type", "current", "--stdin"],
            vec!["send", "current", "--key", "Ctrl+L", "--key", "Enter"],
            vec!["move", "current", "10", "20"],
            vec![
                "click",
                "current",
                "10",
                "20",
                "--count",
                "2",
                "--session",
                "mate",
            ],
            vec!["drag", "current", "10", "20", "30", "40"],
            vec!["scroll", "current", "10", "20", "down"],
        ] {
            parse(&words);
        }
        for words in [
            vec!["send", "current"],
            vec!["type", "current"],
            vec!["type", "current", "hello", "--stdin"],
            vec!["click", "current", "0", "0", "--count", "0"],
            vec!["drag", "current", "0", "0", "1", "1", "--duration-ms", "0"],
            vec!["move", "current", "-1", "0"],
        ] {
            assert!(Cli::try_parse_from(["pbox", "desktop"].into_iter().chain(words)).is_err());
        }
        assert!(
            parse(&["send", "current", "--key", "Enter", "--key", "Typo"])
                .prepare(false)
                .is_err()
        );
        assert!(
            parse(&["screenshot", "current", "--output", "-"])
                .prepare(true)
                .is_err()
        );
        assert!(Cli::try_parse_from(["pbox", "desktop", "current", "--no-viewer"]).is_ok());
        assert!(Cli::try_parse_from(["pbox", "desktop"]).is_ok());
    }

    #[tokio::test]
    async fn click_drag_scroll_release_buttons_and_reject_bad_destinations() {
        for (words, expected) in [
            (
                vec!["click", "box", "1", "1", "--count", "2"],
                vec![0, 1, 0, 1, 0],
            ),
            (
                vec!["drag", "box", "3", "1", "0", "0", "--duration-ms", "40"],
                vec![0, 1, 1, 1, 0],
            ),
            (
                vec!["scroll", "box", "1", "1", "down", "--steps", "2"],
                vec![0, 16, 0, 16, 0],
            ),
        ] {
            let action = parse(&words);
            let (local, remote) = tokio::io::duplex(256);
            let server = tokio::spawn(rfb::tests::server(remote));
            let mut client = rfb::Client::connect(local).await.unwrap();
            execute(&mut client, &action, &[]).await.unwrap();
            let events = server.await.unwrap();
            assert_eq!(events.iter().map(|e| e[1]).collect::<Vec<_>>(), expected);
            if matches!(action, Action::Drag { .. }) {
                assert_eq!(events.last().unwrap(), &[5, 0, 0, 0, 0, 0]);
            }
        }
        let (local, remote) = tokio::io::duplex(256);
        let server = tokio::spawn(rfb::tests::server(remote));
        let mut client = rfb::Client::connect(local).await.unwrap();
        let bad = parse(&["drag", "box", "0", "0", "4", "0"]);
        assert!(execute(&mut client, &bad, &[]).await.is_err());
        client.screen().await.unwrap();
        assert!(server.await.unwrap().is_empty());
    }
}
