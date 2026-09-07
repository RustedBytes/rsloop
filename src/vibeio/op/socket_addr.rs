#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::io;
use std::net::SocketAddr;
#[cfg(unix)]
type NativeAddress = libc::sockaddr_storage;
#[cfg(windows)]
type NativeAddress = windows_sys::Win32::Networking::WinSock::SOCKADDR_STORAGE;

#[inline]
pub(crate) fn socket_addr_to_raw(address: SocketAddr) -> (NativeAddress, socket2::socklen_t) {
    let address = socket2::SockAddr::from(address);
    let length = address.len();
    let mut storage = address.as_storage();
    // SAFETY: NativeAddress is this platform's sockaddr_storage type, as required
    // by view_as. SockAddr::from initializes the storage with a valid IPv4/IPv6
    // address, and the returned value is copied out while storage is alive.
    let native = unsafe { *storage.view_as::<NativeAddress>() };
    (native, length)
}
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_STORAGE,
};

fn validate_address_length(length: usize, expected: usize, capacity: usize) -> io::Result<()> {
    if length < expected || length > capacity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid source address length",
        ));
    }
    Ok(())
}

#[cfg(unix)]
#[inline]
pub(super) fn sockaddr_storage_to_socketaddr(
    storage: &libc::sockaddr_storage,
    length: usize,
) -> Result<SocketAddr, io::Error> {
    let family = storage.ss_family as libc::c_int;

    if family == libc::AF_INET {
        validate_address_length(
            length,
            std::mem::size_of::<libc::sockaddr_in>(),
            std::mem::size_of_val(storage),
        )?;
        // SAFETY: sockaddr_storage has sufficient size/alignment for sockaddr_in;
        // the family and returned length establish that its IPv4 fields are present.
        let addr_in: &libc::sockaddr_in =
            unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
        let port = u16::from_be(addr_in.sin_port);
        let ip_u32 = u32::from_be(addr_in.sin_addr.s_addr);
        let ip = std::net::Ipv4Addr::from(ip_u32);
        Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)))
    } else if family == libc::AF_INET6 {
        validate_address_length(
            length,
            std::mem::size_of::<libc::sockaddr_in6>(),
            std::mem::size_of_val(storage),
        )?;
        // SAFETY: storage is aligned/sized for sockaddr_in6, and both the family
        // and returned length have been checked before borrowing its fields.
        let addr_in6: &libc::sockaddr_in6 =
            unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
        let port = u16::from_be(addr_in6.sin6_port);
        let ip = std::net::Ipv6Addr::from(addr_in6.sin6_addr.s6_addr);
        Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
            ip,
            port,
            addr_in6.sin6_flowinfo,
            addr_in6.sin6_scope_id,
        )))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported socket family",
        ))
    }
}

#[cfg(windows)]
#[inline]
pub(super) fn sockaddr_storage_to_socketaddr(
    storage: &SOCKADDR_STORAGE,
    length: usize,
) -> Result<SocketAddr, io::Error> {
    let family = storage.ss_family;

    if family == AF_INET {
        validate_address_length(
            length,
            std::mem::size_of::<SOCKADDR_IN>(),
            std::mem::size_of_val(storage),
        )?;
        // SAFETY: storage is suitably sized/aligned, and the family/length match IPv4.
        let addr_in: &SOCKADDR_IN = unsafe { &*(storage as *const _ as *const SOCKADDR_IN) };
        let port = u16::from_be(addr_in.sin_port);
        // SAFETY: the IPv4 address union contains initialized network-order bytes.
        let ip_u32 = u32::from_be(unsafe { addr_in.sin_addr.S_un.S_addr });
        let ip = std::net::Ipv4Addr::from(ip_u32);
        Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)))
    } else if family == AF_INET6 {
        validate_address_length(
            length,
            std::mem::size_of::<SOCKADDR_IN6>(),
            std::mem::size_of_val(storage),
        )?;
        // SAFETY: storage is suitably sized/aligned, and the family/length match IPv6.
        let addr_in6: &SOCKADDR_IN6 = unsafe { &*(storage as *const _ as *const SOCKADDR_IN6) };
        let port = u16::from_be(addr_in6.sin6_port);
        // SAFETY: the IPv6 address union contains sixteen initialized address bytes.
        let ip = std::net::Ipv6Addr::from(unsafe { addr_in6.sin6_addr.u.Byte });
        // SAFETY: the validated IPv6 structure includes the initialized scope union.
        let scope_id = unsafe { addr_in6.Anonymous.sin6_scope_id };
        Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
            ip,
            port,
            addr_in6.sin6_flowinfo,
            scope_id,
        )))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported socket family",
        ))
    }
}

#[cfg(any(windows, test))]
pub(super) fn socketaddr_from_buffer(
    buffer: &[u8],
    address: usize,
    length: i32,
) -> io::Result<SocketAddr> {
    // Use the provider's address only to select a checked slice. All reads use
    // the buffer's provenance, including when the supplied address is invalid.
    let bytes = address
        .checked_sub(buffer.as_ptr() as usize)
        .and_then(|offset| {
            let length = usize::try_from(length).ok()?;
            buffer.get(offset..offset.checked_add(length)?)
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "socket address outside output buffer",
            )
        })?;
    #[cfg(unix)]
    type Storage = libc::sockaddr_storage;
    #[cfg(windows)]
    type Storage = SOCKADDR_STORAGE;
    validate_address_length(bytes.len(), 0, std::mem::size_of::<Storage>())?;
    // SAFETY: socket address storage contains integer/byte fields valid when zeroed.
    let mut storage: Storage = unsafe { std::mem::zeroed() };
    // SAFETY: the length is bounded by storage capacity, the source slice is
    // initialized, and the fresh destination cannot overlap it. Byte copying
    // permits unaligned input; the decoder receives properly aligned storage.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            std::ptr::addr_of_mut!(storage).cast::<u8>(),
            bytes.len(),
        );
    }
    sockaddr_storage_to_socketaddr(&storage, bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_addresses_round_trip_with_exact_native_lengths() {
        let addresses = [
            "0.0.0.0:0".parse::<SocketAddr>().unwrap(),
            "192.0.2.123:4321".parse().unwrap(),
            "255.255.255.255:65535".parse().unwrap(),
            "[::]:0".parse().unwrap(),
            "[2001:db8::1234]:65535".parse().unwrap(),
            SocketAddr::V6(std::net::SocketAddrV6::new(
                "fe80::abcd".parse().unwrap(),
                4321,
                0x12345,
                7,
            )),
        ];
        #[cfg(unix)]
        let lengths = [
            std::mem::size_of::<libc::sockaddr_in>(),
            std::mem::size_of::<libc::sockaddr_in6>(),
        ];
        #[cfg(windows)]
        let lengths = [
            std::mem::size_of::<SOCKADDR_IN>(),
            std::mem::size_of::<SOCKADDR_IN6>(),
        ];
        for address in addresses {
            let (storage, length) = socket_addr_to_raw(address);
            assert_eq!(length as usize, lengths[usize::from(address.is_ipv6())]);
            assert_eq!(
                sockaddr_storage_to_socketaddr(&storage, length as usize).unwrap(),
                address
            );
            #[cfg(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "dragonfly",
                target_os = "netbsd"
            ))]
            assert_eq!(storage.ss_len as usize, length as usize);
        }
    }

    #[test]
    fn ipv6_address_fields_survive_unaligned_buffer_decoding() {
        #[cfg(unix)]
        type Address = libc::sockaddr_in6;
        #[cfg(windows)]
        type Address = SOCKADDR_IN6;
        #[cfg(unix)]
        let family = libc::AF_INET6 as libc::sa_family_t;
        #[cfg(windows)]
        let family = AF_INET6;
        #[cfg(unix)]
        let scope_offset = std::mem::offset_of!(Address, sin6_scope_id);
        #[cfg(windows)]
        let scope_offset = std::mem::offset_of!(Address, Anonymous);
        let length = std::mem::size_of::<Address>();
        let mut buffer = vec![0u8; length + 1];
        let mut set = |offset: usize, bytes: &[u8]| {
            buffer[1 + offset..1 + offset + bytes.len()].copy_from_slice(bytes);
        };
        let ip = "fe80::1234".parse::<std::net::Ipv6Addr>().unwrap();
        set(
            std::mem::offset_of!(Address, sin6_family),
            &family.to_ne_bytes(),
        );
        set(
            std::mem::offset_of!(Address, sin6_port),
            &4321u16.to_be_bytes(),
        );
        set(std::mem::offset_of!(Address, sin6_addr), &ip.octets());
        // Match std's sockaddr conversion: flowinfo and scope are preserved,
        // whereas the port is converted from network byte order.
        set(
            std::mem::offset_of!(Address, sin6_flowinfo),
            &0x12345u32.to_ne_bytes(),
        );
        set(scope_offset, &7u32.to_ne_bytes());
        let address =
            socketaddr_from_buffer(&buffer, buffer.as_ptr() as usize + 1, length as i32).unwrap();
        assert_eq!(
            address,
            SocketAddr::V6(std::net::SocketAddrV6::new(ip, 4321, 0x12345, 7))
        );
    }

    #[test]
    fn address_buffer_bounds_and_unaligned_input() {
        #[cfg(unix)]
        type Address = libc::sockaddr_in;
        #[cfg(windows)]
        type Address = SOCKADDR_IN;
        #[cfg(unix)]
        let family = libc::AF_INET as libc::sa_family_t;
        #[cfg(windows)]
        let family = AF_INET;
        let length = std::mem::size_of::<Address>();
        // Deliberately leave just one address at an unaligned offset, with no
        // trailing sockaddr_storage-sized region available to read.
        let mut buffer = vec![0u8; length + 1];
        let family_offset = 1 + std::mem::offset_of!(Address, sin_family);
        let family_bytes = family.to_ne_bytes();
        buffer[family_offset..family_offset + family_bytes.len()].copy_from_slice(&family_bytes);
        let port_offset = 1 + std::mem::offset_of!(Address, sin_port);
        buffer[port_offset..port_offset + 2].copy_from_slice(&4321u16.to_be_bytes());
        let start = buffer.as_ptr() as usize;
        let address = socketaddr_from_buffer(&buffer, start + 1, length as i32).unwrap();
        assert_eq!(address, "0.0.0.0:4321".parse::<SocketAddr>().unwrap());
        for (pointer, size) in [
            (0, length as i32),
            (start - 1, length as i32),
            (start + buffer.len(), length as i32),
            (usize::MAX, length as i32),
            (start + 1, -1),
            (start + 1, 0),
            (start + 1, length as i32 - 1),
            (start + 1, length as i32 + 1),
            (start + 1, i32::MAX),
        ] {
            assert_eq!(
                socketaddr_from_buffer(&buffer, pointer, size)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn source_address_length_is_checked_before_decoding() {
        #[cfg(unix)]
        type Storage = libc::sockaddr_storage;
        #[cfg(windows)]
        type Storage = SOCKADDR_STORAGE;
        #[cfg(unix)]
        let families = [
            (libc::AF_INET, std::mem::size_of::<libc::sockaddr_in>()),
            (libc::AF_INET6, std::mem::size_of::<libc::sockaddr_in6>()),
        ];
        #[cfg(windows)]
        let families = [
            (AF_INET as i32, std::mem::size_of::<SOCKADDR_IN>()),
            (AF_INET6 as i32, std::mem::size_of::<SOCKADDR_IN6>()),
        ];
        for (index, (family, required)) in families.into_iter().enumerate() {
            // SAFETY: socket address storage consists of integer/byte fields;
            // zero initializes its entire storage before the family is assigned.
            let mut storage: Storage = unsafe { std::mem::zeroed() };
            storage.ss_family = family as _;
            let capacity = std::mem::size_of::<Storage>();
            for length in (0..required).chain([capacity + 1, usize::MAX]) {
                assert_eq!(
                    sockaddr_storage_to_socketaddr(&storage, length)
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::InvalidData
                );
            }
            let address = sockaddr_storage_to_socketaddr(&storage, required).unwrap();
            assert_eq!(address.is_ipv4(), index == 0);
            assert!(address.ip().is_unspecified());
            assert_eq!(address.port(), 0);
            for length in required..=capacity {
                assert_eq!(
                    sockaddr_storage_to_socketaddr(&storage, length).unwrap(),
                    address
                );
            }
            storage.ss_family = 0;
            assert_eq!(
                sockaddr_storage_to_socketaddr(&storage, capacity)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
