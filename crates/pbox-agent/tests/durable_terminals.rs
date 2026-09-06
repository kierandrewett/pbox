//! Kill the real network process, then prove the original shell is still alive.
use pbox_agent_client::{AgentClient, ExecRequest};
use pbox_crypto::{
    CertificatePurpose, derive_context_seed, generate_context_ca, issue_certificate, server_subject,
};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Processes {
    children: Vec<Child>,
    root: PathBuf,
}
impl Drop for Processes {
    fn drop(&mut self) {
        for child in self.children.iter_mut().rev() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
#[tokio::test]
async fn agent_process_replacement_preserves_shell_state_and_control() {
    let root = std::env::temp_dir().join(format!("pbox-durable-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut processes = Processes {
        children: Vec::new(),
        root: root.clone(),
    };
    let socket = root.join("terminals/control.sock");
    let executable = env!("CARGO_BIN_EXE_pbox-agent");
    processes.children.push(
        Command::new(executable)
            .arg("--terminal-host")
            .arg(&socket)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let ca = generate_context_ca(&derive_context_seed("durability", "test")).unwrap();
    let id = "pbx_t3yzd9y3";
    let server = issue_certificate(
        &ca,
        &server_subject(id).unwrap(),
        CertificatePurpose::Server,
    )
    .unwrap();
    let identity = issue_certificate(&ca, "test-client", CertificatePurpose::Client).unwrap();
    for (name, contents) in [
        ("ca", &ca.certificate_pem),
        ("cert", &server.certificate_pem),
        ("key", &server.private_key_pem),
    ] {
        std::fs::write(root.join(name), contents).unwrap();
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let spawn = || {
        Command::new(executable)
            .args(["--listen", &address.to_string(), "--box-id", id])
            .arg("--certificate")
            .arg(root.join("cert"))
            .arg("--private-key")
            .arg(root.join("key"))
            .arg("--client-ca")
            .arg(root.join("ca"))
            .env("PBOX_TERMINAL_SOCKET", &socket)
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    };
    processes.children.push(spawn());
    let endpoint = format!("https://{address}");
    let connect = || async {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(client) =
                    AgentClient::connect(&endpoint, id, &ca.certificate_pem, &identity).await
                {
                    break client;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap()
    };
    let mut client = connect().await;
    assert!(
        client
            .info()
            .await
            .unwrap()
            .capabilities
            .iter()
            .any(|c| c == "durable-sessions")
    );
    // The non-workspace root path executes as the test process UID; it does not elevate.
    client.start_session(ExecRequest {
        session_name: "proof".into(),
        argv: vec!["/bin/sh".into(), "-c".into(), "stty -echo; saved=$$; secret=still-alive; cd /tmp; printf 'ready\\n'; while IFS= read -r line; do eval \"$line\"; done".into()],
        cwd: "/tmp".into(), user: "root".into(),
        terminal_rows: 24, terminal_cols: 80, ..Default::default()
    }).await.unwrap();
    async fn screen(client: &mut AgentClient, marker: &str) -> String {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let text = client.read_session("proof").await.unwrap().text;
                if text.contains(marker) {
                    break text;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }
    screen(&mut client, "ready").await;
    // Start an attachment as well; losing it must not close the PTY.
    let attachment = client
        .terminal_session(ExecRequest {
            session_name: "proof".into(),
            argv: vec!["/bin/sh".into()],
            cwd: "/tmp".into(),
            user: "root".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    processes.children[1].kill().unwrap();
    processes.children[1].wait().unwrap();
    drop(attachment);
    drop(client);
    assert!(processes.children[0].try_wait().unwrap().is_none());
    processes.children.push(spawn());
    let mut client = connect().await;
    client
        .send_session(
            "proof",
            "test \"$saved\" = \"$$\" && printf '%s:%s\\n' \"$secret\" \"$PWD\"".into(),
            vec!["Enter".into()],
        )
        .await
        .unwrap();
    let text = screen(&mut client, "still-alive:/tmp").await;
    assert!(text.contains("ready"));
    client.close_session("proof").await.unwrap();
    assert!(client.list_sessions().await.unwrap().is_empty());
}
