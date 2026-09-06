//! Development-account defaults and non-interactive checks of image-provided permissions.
use pbox_agent_client::AgentClient;
use pbox_core::Config;
use std::time::Duration;

/// Only accounts created by pbox receive this policy. Existing image users keep theirs.
pub(crate) const USER_SETUP: &str = r#"
if ! id -u pbox >/dev/null 2>&1; then
  useradd --create-home --shell /bin/bash pbox
  install -d -m 0755 /etc/sudoers.d
  printf '%s\n' 'pbox ALL=(ALL:ALL) NOPASSWD: ALL' > /etc/sudoers.d/90-pbox
  chmod 0440 /etc/sudoers.d/90-pbox
  visudo -cf /etc/sudoers.d/90-pbox
fi
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserAccess {
    ImageDefined,
    Passwordless,
    Restricted,
    Unknown,
}

pub(crate) async fn check_client(client: &mut AgentClient, user: &str) -> UserAccess {
    if matches!(user, "root" | "0") {
        return UserAccess::Passwordless;
    }
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        client.exec(
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                r#"test "$(id -u)" = 0 || sudo -n -u root -- /bin/sh -c 'test "$(id -u)" = 0'"#
                    .to_owned(),
            ],
            "/",
            Vec::<(String, String)>::new(),
            user,
        ),
    )
    .await;
    match result {
        Ok(Ok(result)) if result.exited && result.code == 0 => UserAccess::Passwordless,
        Ok(Ok(result)) if result.exited => UserAccess::Restricted,
        _ => UserAccess::Unknown,
    }
}

pub(crate) fn check(config: &Config, box_id: &str, endpoint: &str) -> UserAccess {
    let Ok(materials) = super::agent_materials(config, box_id) else {
        return UserAccess::Unknown;
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return UserAccess::Unknown;
    };
    runtime.block_on(async {
        let connected = tokio::time::timeout(
            Duration::from_secs(3),
            super::relay::connect_agent(
                config,
                endpoint,
                box_id,
                &materials.ca.certificate_pem,
                &materials.client,
            ),
        )
        .await;
        let Ok(Ok(mut client)) = connected else {
            return UserAccess::Unknown;
        };
        if matches!(tokio::time::timeout(Duration::from_secs(3), client.info()).await,
            Ok(Ok(info)) if info.capabilities.iter().any(|c| c == "workspace"))
        {
            return UserAccess::ImageDefined;
        }
        check_client(&mut client, "pbox").await
    })
}
