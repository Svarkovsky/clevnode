//! Wire protocol for CPython `multiprocessing.connection`
//!
//! Implements length-prefixed framing and bidirectional HMAC handshake
//! as used by Python's `multiprocessing.connection.Listener`/`Client`.
//!
//! Supports both legacy HMAC-MD5 (Python < 3.12) and modern HMAC-SHA256
//! (Python >= 3.12) authentication protocols.

use hmac::{Hmac, Mac};
use md5::Md5;
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::error::RpcError;

type HmacSha256 = Hmac<Sha256>;
type HmacMd5 = Hmac<Md5>;

const CHALLENGE_PREFIX: &[u8] = b"#CHALLENGE#";
const WELCOME: &[u8] = b"#WELCOME#";
const FAILURE: &[u8] = b"#FAILURE#";
const SHA256_DIGEST_TAG: &[u8] = b"{sha256}";
const CHALLENGE_RANDOM_LEN: usize = 40;

/// HMAC-MD5 digest length (16 bytes). Python < 3.12 sends raw 16-byte
/// HMAC-MD5 responses without a digest tag prefix.
const MD5_DIGEST_LEN: usize = 16;
/// HMAC-SHA256 digest length (32 bytes).
const SHA256_DIGEST_LEN: usize = 32;

/// Read a length-prefixed message from a stream.
///
/// Format: `[4-byte big-endian i32 length][payload]`
pub(crate) async fn read_message<R: AsyncRead + Unpin>(
    stream: &mut R,
) -> Result<Vec<u8>, RpcError> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = i32::from_be_bytes(len_buf);

    if len < 0 {
        // Large message format (>= 2 GB), not expected in RPC
        return Err(RpcError::InvalidFormat(
            "large message format not supported".into(),
        ));
    }

    let len = len as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Write a length-prefixed message to a stream.
pub(crate) async fn write_message<W: AsyncWrite + Unpin>(
    stream: &mut W,
    data: &[u8],
) -> Result<(), RpcError> {
    let len = data.len();
    if len > i32::MAX as usize {
        return Err(RpcError::InvalidFormat("message too large".into()));
    }
    let len_buf = (len as i32).to_be_bytes();
    stream.write_all(&len_buf).await?;
    stream.write_all(data).await?;
    stream.flush().await?;
    Ok(())
}

/// Server-side: send challenge, verify client's HMAC response, send WELCOME/FAILURE.
///
/// We always send the modern `{sha256}` challenge. The client's response
/// determines which protocol it speaks:
/// - 16 bytes: legacy HMAC-MD5 (Python < 3.12)
/// - `{sha256}` + 32 bytes: modern HMAC-SHA256 (Python >= 3.12)
pub(crate) async fn deliver_challenge<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    authkey: &[u8; 32],
) -> Result<(), RpcError> {
    // Generate challenge: #CHALLENGE#{sha256} + 40 random bytes
    let mut challenge =
        Vec::with_capacity(CHALLENGE_PREFIX.len() + SHA256_DIGEST_TAG.len() + CHALLENGE_RANDOM_LEN);
    challenge.extend_from_slice(CHALLENGE_PREFIX);
    challenge.extend_from_slice(SHA256_DIGEST_TAG);

    let mut random_bytes = [0u8; CHALLENGE_RANDOM_LEN];
    rand_core::OsRng.fill_bytes(&mut random_bytes);
    challenge.extend_from_slice(&random_bytes);

    write_message(stream, &challenge).await?;

    // Read HMAC response from client
    let response = read_message(stream).await?;

    // message = everything after #CHALLENGE# (includes {sha256} prefix)
    let message = &challenge[CHALLENGE_PREFIX.len()..];

    let verified = if response.len() == MD5_DIGEST_LEN {
        // Legacy HMAC-MD5 response (Python < 3.12):
        // Client computed HMAC-MD5(authkey, "{sha256}" + random_bytes)
        let mut mac = HmacMd5::new_from_slice(authkey)
            .map_err(|e| RpcError::InvalidFormat(format!("HMAC-MD5 init: {}", e)))?;
        mac.update(message);
        mac.verify_slice(&response).is_ok()
    } else if response.starts_with(SHA256_DIGEST_TAG)
        && response.len() == SHA256_DIGEST_TAG.len() + SHA256_DIGEST_LEN
    {
        // Modern HMAC-SHA256 response (Python >= 3.12):
        // Client computed HMAC-SHA256(authkey, "{sha256}" + random_bytes)
        let mut mac = HmacSha256::new_from_slice(authkey)
            .map_err(|e| RpcError::InvalidFormat(format!("HMAC-SHA256 init: {}", e)))?;
        mac.update(message);
        let hmac_bytes = &response[SHA256_DIGEST_TAG.len()..];
        mac.verify_slice(hmac_bytes).is_ok()
    } else {
        false
    };

    if !verified {
        write_message(stream, FAILURE).await?;
        return Err(RpcError::AuthFailed);
    }

    write_message(stream, WELCOME).await?;
    Ok(())
}

/// Server-side: receive client's challenge, compute HMAC, send response, read WELCOME/FAILURE.
///
/// Detects whether the client sent a modern `{sha256}`-prefixed challenge
/// or a legacy challenge (no digest prefix), and responds accordingly:
/// - Modern: HMAC-SHA256 with `{sha256}` prefix in response
/// - Legacy: raw HMAC-MD5 (16 bytes, no prefix)
pub(crate) async fn answer_challenge<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    authkey: &[u8; 32],
) -> Result<(), RpcError> {
    let challenge = read_message(stream).await?;

    // Parse: must start with #CHALLENGE#
    if !challenge.starts_with(CHALLENGE_PREFIX) {
        return Err(RpcError::InvalidFormat("missing #CHALLENGE# prefix".into()));
    }

    let message = &challenge[CHALLENGE_PREFIX.len()..];

    let response = if message.starts_with(SHA256_DIGEST_TAG) {
        // Modern protocol: compute HMAC-SHA256 over full message (including {sha256} prefix)
        let mut mac = HmacSha256::new_from_slice(authkey)
            .map_err(|e| RpcError::InvalidFormat(format!("HMAC-SHA256 init: {}", e)))?;
        mac.update(message);
        let digest = mac.finalize().into_bytes();

        let mut resp = Vec::with_capacity(SHA256_DIGEST_TAG.len() + SHA256_DIGEST_LEN);
        resp.extend_from_slice(SHA256_DIGEST_TAG);
        resp.extend_from_slice(&digest);
        resp
    } else {
        // Legacy protocol (Python < 3.12): compute HMAC-MD5 over raw message
        let mut mac = HmacMd5::new_from_slice(authkey)
            .map_err(|e| RpcError::InvalidFormat(format!("HMAC-MD5 init: {}", e)))?;
        mac.update(message);
        mac.finalize().into_bytes().to_vec()
    };

    write_message(stream, &response).await?;

    // Read WELCOME or FAILURE
    let reply = read_message(stream).await?;
    if reply == WELCOME {
        Ok(())
    } else {
        Err(RpcError::AuthFailed)
    }
}

/// Full server-side handshake: deliver_challenge then answer_challenge.
///
/// Matches Python's `Listener.accept()` which calls:
/// 1. `deliver_challenge(conn, authkey)`, server authenticates client
/// 2. `answer_challenge(conn, authkey)`, server answers client's challenge
pub(crate) async fn server_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    authkey: &[u8; 32],
) -> Result<(), RpcError> {
    deliver_challenge(stream, authkey).await?;
    answer_challenge(stream, authkey).await?;
    Ok(())
}

/// Full client-side handshake: answer_challenge then deliver_challenge.
///
/// Matches Python's `Client()` which calls:
/// 1. `answer_challenge(conn, authkey)`, client answers server's challenge
/// 2. `deliver_challenge(conn, authkey)`, client authenticates server
pub(crate) async fn client_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    authkey: &[u8; 32],
) -> Result<(), RpcError> {
    answer_challenge(stream, authkey).await?;
    deliver_challenge(stream, authkey).await?;
    Ok(())
}

use rand_core::RngCore;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn test_message_round_trip() {
        let (mut client, mut server) = duplex(1024);
        let data = b"hello world";

        write_message(&mut client, data).await.unwrap();
        let received = read_message(&mut server).await.unwrap();
        assert_eq!(received, data);
    }

    #[tokio::test]
    async fn test_empty_message() {
        let (mut client, mut server) = duplex(1024);
        write_message(&mut client, b"").await.unwrap();
        let received = read_message(&mut server).await.unwrap();
        assert!(received.is_empty());
    }

    #[tokio::test]
    async fn test_handshake_success() {
        let authkey = [0x42u8; 32];
        let (mut client, mut server) = duplex(4096);

        let server_task =
            tokio::spawn(async move { server_handshake(&mut server, &authkey).await });
        let client_task =
            tokio::spawn(async move { client_handshake(&mut client, &authkey).await });

        server_task.await.unwrap().unwrap();
        client_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_handshake_wrong_key() {
        let server_key = [0x42u8; 32];
        let client_key = [0x99u8; 32];
        let (mut client, mut server) = duplex(4096);

        let server_task =
            tokio::spawn(async move { server_handshake(&mut server, &server_key).await });
        let client_task =
            tokio::spawn(async move { client_handshake(&mut client, &client_key).await });

        // At least one side should fail
        let (server_result, client_result) = tokio::join!(server_task, client_task);
        let server_err = server_result.unwrap().is_err();
        let client_err = client_result.unwrap().is_err();
        assert!(
            server_err || client_err,
            "mismatched keys must cause auth failure"
        );
    }

    #[tokio::test]
    async fn test_deliver_challenge_bad_response_length() {
        let authkey = [0x42u8; 32];
        let (mut client, mut server) = duplex(4096);

        // Server sends challenge
        let server_task =
            tokio::spawn(async move { deliver_challenge(&mut server, &authkey).await });

        // Client sends wrong-length response
        let client_task = tokio::spawn(async move {
            let _challenge = read_message(&mut client).await.unwrap();
            write_message(&mut client, b"too short").await.unwrap();
            // Read the FAILURE response
            let reply = read_message(&mut client).await.unwrap();
            assert_eq!(reply, FAILURE);
        });

        let server_result = server_task.await.unwrap();
        assert!(server_result.is_err());
        client_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_full_handshake_then_message() {
        let authkey = [0xABu8; 32];
        let (mut client, mut server) = duplex(4096);

        let server_task = tokio::spawn(async move {
            server_handshake(&mut server, &authkey).await.unwrap();
            // Read a request
            let msg = read_message(&mut server).await.unwrap();
            assert_eq!(msg, b"ping");
            // Send a response
            write_message(&mut server, b"pong").await.unwrap();
        });

        let client_task = tokio::spawn(async move {
            client_handshake(&mut client, &authkey).await.unwrap();
            // Send request
            write_message(&mut client, b"ping").await.unwrap();
            // Read response
            let resp = read_message(&mut client).await.unwrap();
            assert_eq!(resp, b"pong");
        });

        server_task.await.unwrap();
        client_task.await.unwrap();
    }

    /// Test that our server can authenticate a legacy HMAC-MD5 client
    /// (simulates Python < 3.12 behavior).
    #[tokio::test]
    async fn test_deliver_challenge_accepts_legacy_md5_client() {
        let authkey = [0x42u8; 32];
        let (mut client, mut server) = duplex(4096);

        let server_task =
            tokio::spawn(async move { deliver_challenge(&mut server, &authkey).await });

        let authkey_clone = authkey;
        let client_task = tokio::spawn(async move {
            // Read challenge from server
            let challenge = read_message(&mut client).await.unwrap();
            assert!(challenge.starts_with(CHALLENGE_PREFIX));

            // Legacy client: strip #CHALLENGE#, compute HMAC-MD5 over remainder
            let message = &challenge[CHALLENGE_PREFIX.len()..];
            let mut mac = HmacMd5::new_from_slice(&authkey_clone).unwrap();
            mac.update(message);
            let digest = mac.finalize().into_bytes();

            // Send raw 16-byte MD5 digest (no prefix)
            write_message(&mut client, &digest).await.unwrap();

            // Should get WELCOME
            let reply = read_message(&mut client).await.unwrap();
            assert_eq!(reply, WELCOME, "server should accept legacy MD5 response");
        });

        server_task.await.unwrap().unwrap();
        client_task.await.unwrap();
    }

    /// Test that our answer_challenge handles legacy challenges (no {sha256} prefix).
    #[tokio::test]
    async fn test_answer_challenge_handles_legacy_challenge() {
        let authkey = [0x42u8; 32];
        let (mut client, mut server) = duplex(4096);

        let authkey_clone = authkey;
        // Simulate a legacy Python < 3.12 server sending challenge without {sha256}
        let server_task = tokio::spawn(async move {
            // Send challenge WITHOUT {sha256} prefix (legacy format)
            let mut challenge = Vec::new();
            challenge.extend_from_slice(CHALLENGE_PREFIX);
            let mut random = [0u8; 20]; // Python < 3.12 uses 20-byte random
            rand_core::OsRng.fill_bytes(&mut random);
            challenge.extend_from_slice(&random);
            write_message(&mut server, &challenge).await.unwrap();

            // Read response, should be raw 16-byte HMAC-MD5
            let response = read_message(&mut server).await.unwrap();
            assert_eq!(
                response.len(),
                MD5_DIGEST_LEN,
                "response to legacy challenge should be raw MD5"
            );

            // Verify the MD5 HMAC
            let message = &challenge[CHALLENGE_PREFIX.len()..];
            let mut mac = HmacMd5::new_from_slice(&authkey_clone).unwrap();
            mac.update(message);
            assert!(
                mac.verify_slice(&response).is_ok(),
                "MD5 HMAC should verify"
            );

            write_message(&mut server, WELCOME).await.unwrap();
        });

        let client_task =
            tokio::spawn(async move { answer_challenge(&mut client, &authkey).await });

        server_task.await.unwrap();
        client_task.await.unwrap().unwrap();
    }

    /// Test full handshake between Rust server and simulated legacy Python < 3.12 client.
    #[tokio::test]
    async fn test_full_handshake_with_legacy_md5_client() {
        let authkey = [0x55u8; 32];
        let (mut client, mut server) = duplex(4096);

        // Server side: normal Rust server
        let server_task =
            tokio::spawn(async move { server_handshake(&mut server, &authkey).await });

        let authkey_clone = authkey;
        // Client side: simulate Python 3.11 (legacy MD5)
        let client_task = tokio::spawn(async move {
            // Phase 1: answer server's challenge with HMAC-MD5
            let challenge = read_message(&mut client).await.unwrap();
            let message = &challenge[CHALLENGE_PREFIX.len()..];
            let mut mac = HmacMd5::new_from_slice(&authkey_clone).unwrap();
            mac.update(message);
            let digest = mac.finalize().into_bytes();
            write_message(&mut client, &digest).await.unwrap();
            let reply = read_message(&mut client).await.unwrap();
            assert_eq!(reply, WELCOME);

            // Phase 2: send own challenge (legacy: no {sha256} prefix)
            let mut challenge2 = Vec::new();
            challenge2.extend_from_slice(CHALLENGE_PREFIX);
            let mut random = [0u8; 20];
            rand_core::OsRng.fill_bytes(&mut random);
            challenge2.extend_from_slice(&random);
            write_message(&mut client, &challenge2).await.unwrap();

            // Read server's response, should be raw MD5
            let response = read_message(&mut client).await.unwrap();
            assert_eq!(response.len(), MD5_DIGEST_LEN);

            let msg = &challenge2[CHALLENGE_PREFIX.len()..];
            let mut mac2 = HmacMd5::new_from_slice(&authkey_clone).unwrap();
            mac2.update(msg);
            assert!(mac2.verify_slice(&response).is_ok());

            write_message(&mut client, WELCOME).await.unwrap();
        });

        server_task.await.unwrap().unwrap();
        client_task.await.unwrap();
    }
}
