//! Extremely basic DNS client.
//!
//! This module provides a very basic asynchronous client-side implementation of
//! the DNS protocol, utilizing an `Async<UdpSocket>` internally. As all of
//! *ground*, this module does not use heap allocation or any
//! locking/synchronization.
//!
//! While it is technically possible to use this implementation to query a
//! public DNS server, it is **not** intended to be used that way. Instead, it
//! is intended to be used to communicate with a **local, trusted**,
//! full-featured, out-of-process DNS resolver (such as systemd-resolved,
//! dnsmasq, or Unbound).
//!
//! Here's a brief, non-exhaustive list of features that you'd expect a
//! production-ready DNS resolver to implement, and which this module **does
//! not** implement:
//!
//! * Respecting `/etc/hosts` and other system configuration
//! * Caching resource records
//! * Multiple upstream servers
//! * Timeouts, retries, automatic failover
//! * Happy Eyeballs
//! * Domain-based request routing (split DNS)
//! * Multicast DNS (mDNS)
//! * DNSSEC
//! * DNS over TLS (DoT)
//! * DNS over HTTPS (DoH)
//!
//! This module is intended to be used to simply forward DNS requests to a
//! proper DNS resolver that implements the features listed above, does all the
//! heavy lifting, and returns back a final, validated response.
//!
//! Please also note that *domain name resolution* is not the same thing as DNS.
//! The former is a general concept of turning a domain name into an address (or
//! metadata), and may use different sources than DNS. When using glibc, domain
//! name resolution is handled by its Name Service Switch (NSS) subsystem, with
//! `nss_dns` being just one of the possible data sources.
//!
//! You might want to implement your own caching of results on top of the
//! caching done by the resolver in use, since local cache lookups can be
//! dramatically faster than talking to another process, even if it runs on the
//! same physical machine. To that end, this module returns TTL values alongside
//! addresses.

use std::{
    convert::{TryFrom, TryInto},
    io::{Error, ErrorKind, Result},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket},
    pin::Pin,
};

use crate::io::{Async, Interest};

/// The Internet DNS class.
const C_IN: u16 = 1;

/// DNS type A: IPv4 address.
const TYPE_A: u16 = 1;
/// DNS type AAAA: IPv6 address.
const TYPE_AAAA: u16 = 28;

/// IP address family.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AddressFamily {
    /// IP version 4 address.
    IPv4,
    /// IP version 6 address.
    IPv6,
}

/// IP address record with TTL.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Record {
    /// The address.
    pub address: IpAddr,
    /// Time to live, in seconds.
    ///
    /// It's OK to cache this record for this long.
    pub ttl: u32,
}

impl AddressFamily {
    fn as_type(self) -> u16 {
        match self {
            AddressFamily::IPv4 => TYPE_A,
            AddressFamily::IPv6 => TYPE_AAAA,
        }
    }
}

fn make_query_packet(name: &[u8], mut buffer: &mut [u8], query_type: u16) -> usize {
    // FIXME: This should not panic when there's not enough space in the buffer.
    let original_buffer_size = buffer.len();

    macro_rules! put {
        ($value:expr) => {
            let size = std::mem::size_of_val(&$value);
            buffer[..size].copy_from_slice(&$value.to_be_bytes());
            buffer = &mut buffer[size..];
        };
    }

    put!(0u16); // ID
    put!(0x0120u16); // flags
    put!(1u16); // question count
    put!(0u16); // answer count
    put!(0u16); // authority count
    put!(0u16); // additional count

    for label in name.split(|&b| b == b'.') {
        if label.is_empty() {
            continue;
        }
        let len = label.len();
        assert!(len < 128);
        put!(len as u8);
        buffer[..len].copy_from_slice(label);
        buffer = &mut buffer[len..];
    }
    put!(0u8);

    put!(query_type);
    put!(C_IN); // class

    original_buffer_size - buffer.len()
}

fn name_size(buffer: &[u8]) -> Result<usize> {
    let mut pos = 0;
    loop {
        let byte = match buffer.get(pos) {
            Some(&byte) => byte,
            None => return Err(Error::from(ErrorKind::InvalidData)),
        };
        pos += 1;
        match byte {
            0 => return Ok(pos),
            1..=127 => pos += byte as usize,
            _ if pos + 1 <= buffer.len() => return Ok(pos + 1),
            _ => return Err(Error::from(ErrorKind::InvalidData)),
        }
    }
}

fn parse_response_packet(buffer: &[u8]) -> Result<Records<'_>> {
    if buffer.len() < 12 {
        return Err(Error::from(ErrorKind::InvalidData));
    }

    let flags = u16::from_be_bytes(buffer[2..4].try_into().unwrap());
    let question_count = u16::from_be_bytes(buffer[4..6].try_into().unwrap()) as usize;
    let answer_count = u16::from_be_bytes(buffer[6..8].try_into().unwrap()) as usize;

    if flags & 0x8000 != 0x8000 {
        // It's a query, not an answer?
        return Err(Error::from(ErrorKind::InvalidData));
    }
    if flags & 0xf == 3 || answer_count == 0 {
        return Err(Error::from(ErrorKind::NotFound));
    }
    if flags & 0xf != 0 {
        return Err(Error::from(ErrorKind::Other));
    }

    // Skip the questions.
    let mut pos = 12;
    for _ in 0..question_count {
        pos += name_size(&buffer[pos..])?;
        pos += 4;
        // The header declared that there are some answers, so we don't expect
        // to run into the end of packet.
        if pos >= buffer.len() {
            return Err(Error::from(ErrorKind::InvalidData));
        }
    }

    Ok(Records {
        buffer: &buffer[pos..],
        answer_count,
    })
}

/// Performs a DNS query.
///
/// This function builds a DNS query packet in the provided buffer, then sends
/// it to the specified DNS server. Then it waits for a reply and does some
/// basic parsing of the reply packet to extract contained addresses.
///
/// See [the module level documentation](index.html) for a discussion of how
/// this function should and should not be used.
///
/// Note the implementation needs a buffer to build the query DNS packet and to
/// receive and parse the response DNS packet. While it could simply allocate
/// the buffer on the stack, any such stack allocation contributes to the size
/// of the resulting future. It might be beneficial to allocate the buffer on
/// the heap after all, to only take up the memory while the DNS query is
/// ongoing, or to use some more sophisticated memory management scheme. This
/// decision is left to the caller.
///
/// # Arguments
///
/// * `name` - The domain name to look up.
/// * `address_family` - Whether to look up IPv4 or IPv6 addresses.
/// * `dns_server` - Which DNS server to query.
/// * `buffer` - A caller-provided buffer for internal usage.
///
/// # Examples
///
/// Query a DNS server at `127.0.0.53:53` for IPv6 addresses of `example.org`:
/// ```
/// use ground::dns::AddressFamily;
///
/// # executor::run(|_| async {
/// let mut buffer = [0u8; 512];
///
/// let records = ground::dns::query(
///     b"example.org",
///     AddressFamily::IPv6,
///     "127.0.0.53:53".parse().unwrap(),
///     &mut buffer
/// ).await?;
///
/// for record in records {
///     let record = record?;
///     println!("{} with a TTL of {}", record.address, record.ttl);
/// }
///
/// # Ok::<(), std::io::Error>(())
/// # }).unwrap();
/// ```
///
/// A query for a non-existent domain returns [`NotFound`]:
/// ```
/// use std::io::ErrorKind;
/// use ground::dns::AddressFamily;
///
/// # executor::run(|_| async {
/// let mut buffer = [0u8; 512];
///
/// let result = ground::dns::query(
///     b"no.example.org",
///     AddressFamily::IPv4,
///     "127.0.0.53:53".parse().unwrap(),
///     &mut buffer
/// ).await;
///
/// assert_eq!(result.unwrap_err().kind(), ErrorKind::NotFound);
/// # });
/// ```
///
/// [`NotFound`]: https://doc.rust-lang.org/std/io/enum.ErrorKind.html#variant.NotFound
pub async fn query<'a>(
    name: &[u8],
    address_family: AddressFamily,
    dns_server: SocketAddr,
    buffer: &'a mut [u8],
) -> Result<Records<'a>> {
    let buffer_size = make_query_packet(name, buffer, address_family.as_type());
    let send_buffer = &buffer[..buffer_size];

    let unspecified_sockaddr = match dns_server {
        SocketAddr::V4(_) => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0)),
    };
    // FIXME: This is doing an extra bind syscall that we don't want.
    let socket = UdpSocket::bind(unspecified_sockaddr)?;
    let mut socket = Async::new(socket);
    // SAFETY: We're not moving the socket. This is just like pin_mut!(), except
    // we don't use the macro because we don't want to depend on the whole
    // futures crate.
    let mut socket = unsafe { Pin::new_unchecked(&mut socket) };

    socket
        .as_mut()
        .nonblocking(Interest::Write, |s| s.send_to(send_buffer, &dns_server))
        .await?;
    loop {
        // Receive a packet.
        let (size, sender) = socket
            .as_mut()
            .nonblocking(Interest::Read, |s| s.recv_from(buffer))
            .await?;
        // If it's from someone else, drop it and continue. Note that this is
        // not a security measure, as it's trivial to spoof the sender address.
        if sender != dns_server {
            continue;
        }
        return parse_response_packet(&buffer[..size]);
    }
}

/// An iterator over [`Record`]s.
#[derive(Clone, Debug)]
pub struct Records<'a> {
    buffer: &'a [u8],
    answer_count: usize,
}

impl<'a> Iterator for Records<'a> {
    type Item = Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.answer_count > 0 {
            let name_size = match name_size(self.buffer) {
                Ok(name_size) => name_size,
                Err(e) => return Some(Err(e)),
            };
            self.buffer = &self.buffer[name_size..];
            if self.buffer.len() < 10 {
                return Some(Err(Error::from(ErrorKind::InvalidData)));
            }
            let rr_type = u16::from_be_bytes(self.buffer[..2].try_into().unwrap());
            let class = u16::from_be_bytes(self.buffer[2..4].try_into().unwrap());
            let ttl = u32::from_be_bytes(self.buffer[4..8].try_into().unwrap());
            let data_size = u16::from_be_bytes(self.buffer[8..10].try_into().unwrap()) as usize;
            if self.buffer.len() < 10 + data_size {
                return Some(Err(Error::from(ErrorKind::InvalidData)));
            }
            let data = &self.buffer[10..10 + data_size];
            self.buffer = &self.buffer[10 + data_size..];
            self.answer_count -= 1;

            if class != C_IN {
                continue;
            }

            match rr_type {
                TYPE_A => {
                    let data = match <[u8; 4]>::try_from(data) {
                        Ok(data) => data,
                        Err(_) => return Some(Err(Error::from(ErrorKind::InvalidData))),
                    };
                    let record = Record {
                        address: IpAddr::V4(Ipv4Addr::from(data)),
                        ttl,
                    };
                    return Some(Ok(record));
                }
                TYPE_AAAA => {
                    let data = match <[u8; 16]>::try_from(data) {
                        Ok(data) => data,
                        Err(_) => return Some(Err(Error::from(ErrorKind::InvalidData))),
                    };
                    let record = Record {
                        address: IpAddr::V6(Ipv6Addr::from(data)),
                        ttl,
                    };
                    return Some(Ok(record));
                }
                _ => continue,
            }
        }
        return None;
    }
}
