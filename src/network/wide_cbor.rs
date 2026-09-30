// Request-response CBOR codec, wire-identical to libp2p's `request_response::cbor`
// (same cbor4ii serde encoding, same framing), but with larger read limits.
//
// Why: libp2p's codec caps a response at 10 MiB. Vec<u8> fields (ML-DSA
// signatures/keys) encode as CBOR integer arrays at ~1.9 bytes per byte, so
// any block or transaction above ~5.3 MB raw -- still valid under the 8 MiB
// consensus limit -- could not be downloaded, stalling this node behind it.
// Only the read side changes, so peers running the stock codec interoperate.

use libp2p::futures::prelude::*;
use libp2p::StreamProtocol;
use serde::{de::DeserializeOwned, Serialize};
use std::{collections::TryReserveError, convert::Infallible, io, marker::PhantomData};

const REQUEST_SIZE_MAXIMUM: u64 = 4 * 1024 * 1024;
const RESPONSE_SIZE_MAXIMUM: u64 = 64 * 1024 * 1024;

pub struct Codec<Req, Resp> {
    phantom: PhantomData<(Req, Resp)>,
}

impl<Req, Resp> Default for Codec<Req, Resp> {
    fn default() -> Self {
        Codec { phantom: PhantomData }
    }
}

impl<Req, Resp> Clone for Codec<Req, Resp> {
    fn clone(&self) -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl<Req, Resp> libp2p::request_response::Codec for Codec<Req, Resp>
where
    Req: Send + Serialize + DeserializeOwned,
    Resp: Send + Serialize + DeserializeOwned,
{
    type Protocol = StreamProtocol;
    type Request = Req;
    type Response = Resp;

    async fn read_request<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<Req>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut vec = Vec::new();
        io.take(REQUEST_SIZE_MAXIMUM).read_to_end(&mut vec).await?;
        cbor4ii::serde::from_slice(vec.as_slice()).map_err(decode_into_io_error)
    }

    async fn read_response<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<Resp>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut vec = Vec::new();
        io.take(RESPONSE_SIZE_MAXIMUM).read_to_end(&mut vec).await?;
        cbor4ii::serde::from_slice(vec.as_slice()).map_err(decode_into_io_error)
    }

    async fn write_request<T>(&mut self, _: &Self::Protocol, io: &mut T, req: Self::Request) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let data: Vec<u8> = cbor4ii::serde::to_vec(Vec::new(), &req).map_err(encode_into_io_error)?;
        io.write_all(data.as_ref()).await?;
        Ok(())
    }

    async fn write_response<T>(&mut self, _: &Self::Protocol, io: &mut T, resp: Self::Response) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let data: Vec<u8> = cbor4ii::serde::to_vec(Vec::new(), &resp).map_err(encode_into_io_error)?;
        io.write_all(data.as_ref()).await?;
        Ok(())
    }
}

fn decode_into_io_error(err: cbor4ii::serde::DecodeError<Infallible>) -> io::Error {
    use cbor4ii::core::error::DecodeError;
    match err {
        cbor4ii::serde::DecodeError::Core(DecodeError::Read(e)) => io::Error::new(io::ErrorKind::Other, e),
        cbor4ii::serde::DecodeError::Core(e @ DecodeError::Unsupported { .. }) => io::Error::new(io::ErrorKind::Unsupported, e),
        cbor4ii::serde::DecodeError::Core(e @ DecodeError::Eof { .. }) => io::Error::new(io::ErrorKind::UnexpectedEof, e),
        cbor4ii::serde::DecodeError::Core(e) => io::Error::new(io::ErrorKind::InvalidData, e),
        cbor4ii::serde::DecodeError::Custom(e) => io::Error::new(io::ErrorKind::Other, e.to_string()),
    }
}

fn encode_into_io_error(err: cbor4ii::serde::EncodeError<TryReserveError>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::request_response::Codec as _;

    #[tokio::test]
    async fn round_trips_responses_over_the_stock_10mib_limit() {
        // ~6 MB of signature-like bytes -> ~11.4 MB of CBOR, above the stock limit.
        let payload: Vec<Vec<u8>> = vec![(0..6_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect()];
        let proto = StreamProtocol::new("/qbtc/test/1");
        let mut codec = Codec::<Vec<u8>, Vec<Vec<u8>>>::default();
        let mut buf = Vec::new();
        codec.write_response(&proto, &mut libp2p::futures::io::Cursor::new(&mut buf), payload.clone()).await.unwrap();
        assert!(buf.len() > 10 * 1024 * 1024, "encoded {} bytes", buf.len());
        let got = codec.read_response(&proto, &mut libp2p::futures::io::Cursor::new(&buf)).await.unwrap();
        assert_eq!(got, payload);
    }
}
