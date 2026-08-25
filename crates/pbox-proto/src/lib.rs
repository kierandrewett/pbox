pub const PROTOCOL_VERSION: u32 = 3;

pub mod agent {
    tonic::include_proto!("pbox.cwd.dev.agent.v1");
}
