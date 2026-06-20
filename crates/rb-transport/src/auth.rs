//! Authentication for rb-transport.
//! Vendored from bore, adapted for minimal proto messages.

use anyhow::{ensure, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use uuid::Uuid;

use crate::proto::{ClientMsg, Delimited, ServerMsg};

#[derive(Clone)]
pub struct Authenticator(Hmac<Sha256>);

impl Authenticator {
    pub fn new(secret: &str) -> Self {
        let hashed_secret = Sha256::new().chain_update(secret).finalize();
        Self(Hmac::new_from_slice(&hashed_secret).expect("HMAC can take key of any size"))
    }

    pub fn answer(&self, challenge: &Uuid) -> String {
        let mut hmac = self.0.clone();
        hmac.update(challenge.as_bytes());
        hex::encode(hmac.finalize().into_bytes())
    }

    pub fn validate(&self, challenge: &Uuid, tag: &str) -> bool {
        if let Ok(tag) = hex::decode(tag) {
            let mut hmac = self.0.clone();
            hmac.update(challenge.as_bytes());
            hmac.verify_slice(&tag).is_ok()
        } else {
            false
        }
    }

    pub async fn server_handshake<T: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: &mut Delimited<T>,
    ) -> Result<()> {
        let challenge = Uuid::new_v4();
        stream.send_server(ServerMsg::Challenge(challenge)).await?;
        match stream.recv_client().await? {
            Some(ClientMsg::Authenticate(tag)) => {
                ensure!(self.validate(&challenge, &tag), "invalid secret");
                Ok(())
            }
            _ => anyhow::bail!("server requires secret, but no secret was provided"),
        }
    }

    pub async fn client_handshake<T: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: &mut Delimited<T>,
    ) -> Result<()> {
        let challenge = match stream.recv_server().await? {
            Some(ServerMsg::Challenge(challenge)) => challenge,
            Some(_) => {
                anyhow::bail!("expected authentication challenge, but no secret was required")
            }
            None => anyhow::bail!("connection closed before authentication"),
        };
        let tag = self.answer(&challenge);
        stream.send_client(ClientMsg::Authenticate(tag)).await?;
        Ok(())
    }
}
