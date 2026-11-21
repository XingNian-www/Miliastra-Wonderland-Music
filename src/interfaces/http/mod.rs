pub(crate) mod facade;
mod panel;
mod ports;
pub(crate) use ports::HttpDebugRunRequest;
mod protocol;

pub(crate) use ports::{
    HttpAiMemoryPort, HttpAiMusicPort, HttpApplicationPorts, HttpCommandError, HttpCommandPort,
    HttpDebugPort, HttpHallPort, HttpLoginError, HttpLoginErrorView, HttpLoginPort,
    HttpLoginQrCodeView, HttpLoginStatus, HttpPlayerPort, HttpPlayerSearchError, HttpProviderView,
    HttpQueryPort, HttpTaskPort, PlayTrackRequest,
};
pub(crate) use protocol::{
    HttpInterfaceConfig, HttpServer, HttpSharedState, WebToolRequest, WebToolTemplate, start,
};
