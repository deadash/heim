use heim_common::prelude::*;
use winapi::um::iptypes::GAA_FLAG_INCLUDE_GATEWAYS;
use winapi::um::iptypes::IP_ADAPTER_DHCP_ENABLED;

use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;

use std::ffi::CStr;
use widestring::UCStr;

use winapi::shared::ifdef::IfOperStatusUp;
use winapi::shared::minwindef::ULONG;
use winapi::shared::ntdef::NULL;
use winapi::shared::winerror::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
use winapi::shared::ws2def::AF_UNSPEC;
use winapi::shared::ws2def::SOCKADDR_IN;
use winapi::shared::ws2def::SOCKET_ADDRESS;
use winapi::shared::ws2def::{AF_INET, AF_INET6};
use winapi::shared::ws2ipdef::SOCKADDR_IN6;
use winapi::um::iphlpapi::GetAdaptersAddresses;
use winapi::um::iptypes::GAA_FLAG_INCLUDE_PREFIX;
use winapi::um::iptypes::IP_ADAPTER_ADDRESSES;
use winapi::um::iptypes::PIP_ADAPTER_ADDRESSES;

use crate::Address;

#[derive(Clone, Debug)]
pub struct Nic {
    index: u32,
    guid: String,
    friendly_name: String,
    is_up: bool,
    address: Option<Address>,
    netmask: Option<Address>,
    gateway: Option<Address>,
    if_type: u32,
    mac_address: String,
    is_dhcp: bool,
}

fn sockaddr_to_ipv4(sa: SOCKET_ADDRESS) -> Option<Address> {
    // Check this sockaddr can fit one short and a char[14]
    // (see https://docs.microsoft.com/en-us/windows/win32/winsock/sockaddr-2)
    // This should always happen though, this is guaranteed by winapi's interface
    if (sa.iSockaddrLength as usize) < std::mem::size_of::<SOCKADDR_IN>() {
        return None;
    }

    if sa.lpSockaddr.is_null() {
        return None;
    }
    let arr = unsafe { (*sa.lpSockaddr).sa_data };
    let ip4 = Ipv4Addr::new(arr[2] as _, arr[3] as _, arr[4] as _, arr[5] as _);
    let port = (arr[0] as u16) + (arr[1] as u16) * 0x100;

    Some(Address::Inet(SocketAddrV4::new(ip4, port)))
}

fn sockaddr_to_ipv6(sa: SOCKET_ADDRESS) -> Option<Address> {
    // Check this sockaddr can fit a SOCKADDR_IN6 (two shorts, two longs, and a 16-byte struct)
    // (see https://docs.microsoft.com/en-us/windows/win32/winsock/sockaddr-2)
    if (sa.iSockaddrLength as usize) < std::mem::size_of::<SOCKADDR_IN6>() {
        return None;
    }

    let p_sa6 = sa.lpSockaddr as *const SOCKADDR_IN6;
    if p_sa6.is_null() {
        return None;
    }
    let sa6 = unsafe { *p_sa6 };

    let ip6_data = unsafe { sa6.sin6_addr.u.Byte() };
    let ip6 = Ipv6Addr::from(*ip6_data);
    let port = sa6.sin6_port;
    let flow_info = sa6.sin6_flowinfo;
    let scope_id = unsafe { *sa6.u.sin6_scope_id() };

    Some(Address::Inet6(SocketAddrV6::new(
        ip6, port, flow_info, scope_id,
    )))
}

/// Generate an IPv4 netmask from a prefix length (Rust equivalent of ConvertLengthToIpv4Mask())
fn ipv4_netmask_from(length: u8) -> Ipv4Addr {
    let mask = match length {
        len if len <= 32 => u32::max_value().checked_shl(32 - len as u32).unwrap_or(0),
        _ /* invalid value */ => u32::max_value(),
    };
    Ipv4Addr::from(mask)
}

/// Generate an IPv6 netmask from a prefix length
fn ipv6_netmask_from(length: u8) -> Ipv6Addr {
    let mask = match length {
        len if len <= 128 => u128::max_value().checked_shl(128 - len as u32).unwrap_or(0),
        _ /* invalid value */ => u128::max_value(),
    };
    Ipv6Addr::from(mask)
}

fn ipv4_netmask_address_from(length: u8) -> Address {
    Address::Inet(SocketAddrV4::new(ipv4_netmask_from(length), 0))
}
fn ipv6_netmask_address_from(length: u8) -> Address {
    Address::Inet6(SocketAddrV6::new(ipv6_netmask_from(length), 0, 0, 0))
}

impl Nic {
    pub fn name(&self) -> &str {
        &self.friendly_name
    }

    pub fn index(&self) -> Option<u32> {
        Some(self.index)
    }

    pub fn guid(&self) -> &str {
        &self.guid
    }

    pub fn address(&self) -> Address {
        self.address
            .unwrap_or_else(|| Address::Inet(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0)))
    }

    pub fn netmask(&self) -> Option<Address> {
        self.netmask
    }

    pub fn destination(&self) -> Option<Address> {
        // TODO: we could implement something one day
        self.gateway
    }

    pub fn is_up(&self) -> bool {
        self.is_up
    }

    pub fn is_running(&self) -> bool {
        // TODO: not sure how to tell on Windows
        true
    }

    pub fn is_loopback(&self) -> bool {
        match self.address {
            Some(Address::Inet(sa)) => sa.ip().is_loopback(),
            Some(Address::Inet6(sa6)) => sa6.ip().is_loopback(),
            _ => false,
        }
    }

    pub fn is_multicast(&self) -> bool {
        match self.address {
            Some(Address::Inet(sa)) => sa.ip().is_multicast(),
            Some(Address::Inet6(sa6)) => sa6.ip().is_multicast(),
            _ => false,
        }
    }

    pub fn if_type(&self) -> u32 {
        self.if_type
    }

    pub fn mac_address(&self) -> &str {
        &self.mac_address
    }

    pub fn is_dhcp(&self) -> bool {
        self.is_dhcp
    }
}

fn format_mac_address(mac: &[u8], len: ULONG) -> String {
    mac.iter()
        .take(len as usize)
        .map(|byte| format!("{:02X}", byte))
        .collect::<Vec<String>>()
        .join(":")
}

pub async fn nic() -> Result<impl Stream<Item = Result<Nic>> + Send + Sync> {
    let mut results = Vec::new();

    // Step 1 - get the size of the routing infos
    let family = AF_UNSPEC; // retrieve both IPv4 and IPv6 interfaces
    let flags: ULONG = GAA_FLAG_INCLUDE_PREFIX | GAA_FLAG_INCLUDE_GATEWAYS;
    let mut empty_list: IP_ADAPTER_ADDRESSES = unsafe { std::mem::zeroed() };
    let mut data_size: ULONG = 0;
    let res =
        unsafe { GetAdaptersAddresses(family as _, flags, NULL, &mut empty_list, &mut data_size) };
    if res != ERROR_BUFFER_OVERFLOW {
        // Unable to get the size of routing infos
        let e = Error::from(std::io::Error::from_raw_os_error(res as _))
            .with_ffi("GetAdaptersAddresses");
        return Err(e);
    }

    // Step 2 - get the interfaces infos
    let mut buffer = vec![0; data_size as usize];
    let res = unsafe {
        GetAdaptersAddresses(
            family as _,
            flags,
            NULL,
            buffer.as_mut_ptr() as _,
            &mut data_size,
        )
    };
    if res != NO_ERROR {
        // Unable to get the routing infos
        let e = Error::from(std::io::Error::from_raw_os_error(res as _))
            .with_ffi("GetAdaptersAddresses");
        return Err(e);
    }

    // Step 3 - walk through the list and populate our interfaces
    let mut cur_iface = unsafe {
        let p = buffer.as_ptr() as PIP_ADAPTER_ADDRESSES;
        if p.is_null() {
            // Unable to list interfaces
            let e = Error::from(std::io::Error::from_raw_os_error(res as _))
                .with_ffi("GetAdaptersAddresses");
            return Err(e);
        }
        *p
    };

    loop {
        let iface_index;
        let iface_guid_cstr;
        let iface_fname_ucstr;
        let is_up;
        let iface_tye;
        let mac_address;
        let is_dhcp;

        unsafe {
            iface_index = cur_iface.u.s().IfIndex;
            iface_guid_cstr = CStr::from_ptr(cur_iface.AdapterName);
            iface_fname_ucstr = UCStr::from_ptr_str(cur_iface.FriendlyName);
            is_up = cur_iface.OperStatus == IfOperStatusUp;
            iface_tye = cur_iface.IfType;
            mac_address = format_mac_address(&cur_iface.PhysicalAddress, cur_iface.PhysicalAddressLength);
            is_dhcp = cur_iface.Flags & IP_ADAPTER_DHCP_ENABLED != 0;
        }
        let iface_guid = iface_guid_cstr
            .to_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "".into());
        let iface_friendly_name = iface_fname_ucstr.to_string_lossy();

        let base_nic = Nic {
            index: iface_index,
            friendly_name: iface_friendly_name,
            guid: iface_guid,
            is_up,
            address: None,
            netmask: None,
            gateway: None,
            if_type: iface_tye,
            mac_address,
            is_dhcp,
        };
 
        let mut ipv4_gateway_addresses: Vec<Address> = Vec::new();
        let mut ipv6_gateway_addresses: Vec<Address> = Vec::new();
        unsafe {
            let mut gateway_struct = cur_iface.FirstGatewayAddress;
            while !gateway_struct.is_null() {
                let gateway_struct_ref = &*gateway_struct;
                let sa_family = (*gateway_struct_ref.Address.lpSockaddr).sa_family as i32;
    
                match sa_family {
                    AF_INET => {
                        if let Some(gateway) = sockaddr_to_ipv4(gateway_struct_ref.Address) {
                            ipv4_gateway_addresses.push(gateway); // 收集IPv4网关地址
                        }
                    },
                    AF_INET6 => {
                        if let Some(gateway) = sockaddr_to_ipv6(gateway_struct_ref.Address) {
                            ipv6_gateway_addresses.push(gateway); // 收集IPv6网关地址
                        }
                    },
                    _ => {}
                }
    
                gateway_struct = gateway_struct_ref.Next;
            }
        }
        let mut cur_address_ptr = cur_iface.FirstUnicastAddress;
        // Walk through every IP address of this interface
        while !cur_address_ptr.is_null() {
            let cur_address = unsafe { &*cur_address_ptr };
            
            let this_socket_address = cur_address.Address;
            let this_netmask_length = cur_address.OnLinkPrefixLength;
            let this_sa_family = unsafe { (*this_socket_address.lpSockaddr).sa_family };

            let (this_address, this_netmask, this_gateway) = match this_sa_family as i32 {
                AF_INET => (
                    sockaddr_to_ipv4(this_socket_address),
                    Some(ipv4_netmask_address_from(this_netmask_length)),
                    ipv4_gateway_addresses.first().copied(),
                ),
                AF_INET6 => (
                    sockaddr_to_ipv6(this_socket_address),
                    Some(ipv6_netmask_address_from(this_netmask_length)),
                    ipv6_gateway_addresses.first().copied(),
                ),
                _ => (None, None, None),
            };

            let mut this_nic = base_nic.clone();
            this_nic.address = this_address;
            this_nic.netmask = this_netmask;
            this_nic.gateway = this_gateway;
            results.push(Ok(this_nic));

            cur_address_ptr = cur_address.Next;
        }

        let next_item = cur_iface.Next;
        if next_item.is_null() {
            break;
        }
        cur_iface = unsafe { *next_item };
    }

    Ok(stream::iter(results))
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test_netmasks() {
        assert_eq!(ipv4_netmask_from(0), Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(ipv4_netmask_from(32), Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(ipv4_netmask_from(200), Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(ipv4_netmask_from(9), Ipv4Addr::new(255, 128, 0, 0));

        assert_eq!(ipv6_netmask_from(0), Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0));
        assert_eq!(
            ipv6_netmask_from(32),
            Ipv6Addr::new(0xffff, 0xffff, 0, 0, 0, 0, 0, 0)
        );
        assert_eq!(
            ipv6_netmask_from(200),
            Ipv6Addr::new(0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff)
        );
        assert_eq!(
            ipv6_netmask_from(9),
            Ipv6Addr::new(0xff80, 0, 0, 0, 0, 0, 0, 0)
        );
    }
}
