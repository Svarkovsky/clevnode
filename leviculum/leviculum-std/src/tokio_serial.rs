//! Mock implementation of tokio_serial to avoid compiling the heavy original dependency

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DataBits { Five, Six, Seven, Eight }

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Parity { None, Even, Odd }

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StopBits { One, Two }

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FlowControl { None, Software, Hardware }

#[derive(Debug)]
pub struct Error;

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Mock Serial Error")
    }
}

impl std::error::Error for Error {}

pub fn new(_port: &str, _baud: u32) -> Builder {
    Builder
}

pub struct Builder;

impl Builder {
    pub fn data_bits(self, _v: DataBits) -> Self { self }
    pub fn stop_bits(self, _v: StopBits) -> Self { self }
    pub fn parity(self, _v: Parity) -> Self { self }
    pub fn flow_control(self, _v: FlowControl) -> Self { self }
}

pub struct SerialStream;

impl SerialStream {
    pub fn open(_builder: &Builder) -> Result<Self, Error> {
        Err(Error)
    }
}

impl tokio::io::AsyncRead for SerialStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Serial ports are disabled in this build",
        )))
    }
}

impl tokio::io::AsyncWrite for SerialStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Serial ports are disabled in this build",
        )))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}
