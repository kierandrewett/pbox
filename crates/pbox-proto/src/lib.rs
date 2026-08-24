pub const PROTOCOL_VERSION: u32 = 2;

pub mod agent {
    tonic::include_proto!("pbox.cwd.dev.agent.v1");
}
