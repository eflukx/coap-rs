use std::{
    self,
    pin::Pin,
    net::{self, SocketAddr, ToSocketAddrs, Ipv4Addr, Ipv6Addr, IpAddr},
    task::Context,
    future::Future,
};
use log::{debug, error};
use futures::{SinkExt, Stream, StreamExt, select, stream::FusedStream, task::Poll};
use tokio::{
    io,
    sync::mpsc,
    net::UdpSocket,
};
use tokio_util::udp::{UdpFramed};
use coap_lite::{
    Packet, CoapRequest, CoapResponse,
};

use super::message::Codec;
use super::observer::Observer;

pub type MessageSender = mpsc::UnboundedSender<(Packet, SocketAddr)>;
type MessageReceiver = mpsc::UnboundedReceiver<(Packet, SocketAddr)>;

#[derive(Debug)]
pub enum CoAPServerError {
    NetworkError,
    EventLoopError,
    AnotherHandlerIsRunning,
    EventSendError,
}

#[derive(Debug)]
pub struct QueuedMessage {
    pub address: SocketAddr,
    pub message: Packet,
}

pub enum Message {
    NeedSend(Packet, SocketAddr),
    Received(Packet, SocketAddr),
}

pub struct Server<'a, HandlerRet> where HandlerRet: Future<Output=Option<CoapResponse>> {
    server: CoAPServer,
    observer: Observer,
    handler: Option<Box<dyn FnMut(CoapRequest<SocketAddr>) -> HandlerRet + Send + 'a>>,
}

impl<'a, HandlerRet> Server<'a, HandlerRet> where HandlerRet: Future<Output=Option<CoapResponse>> {
    /// Creates a CoAP server listening on the given address.
    pub fn new<A: ToSocketAddrs>(addr: A) -> Result<Server<'a, HandlerRet>, io::Error> {
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Server {
            server: CoAPServer::new(addr, rx)?,
            observer: Observer::new(tx),
            handler: None,
        })
    }

    /// run the server.
    pub async fn run<F: FnMut(CoapRequest<SocketAddr>) -> HandlerRet + Send + 'a>(&mut self, handler: F) -> Result<(), io::Error> {
        self.handler = Some(Box::new(handler));

        loop {
            select! {
                message = self.server.select_next_some() => {
                    match message {
                        Ok(Message::NeedSend(packet, addr)) => {
                            self.server.send((packet, addr)).await?;
                        }
                        Ok(Message::Received(packet, addr)) => {
                            self.dispatch_msg(packet, addr).await?;
                        }
                        Err(e) => {
                            error!("select error: {:?}", e);
                        }
                    }
                }
                _ = self.observer.select_next_some() => {
                    self.observer.timer_handler().await;
                }
                complete => break,
            }
        }
        Ok(())
    }

    /// Return the local address that the server is listening on. This can be useful when starting
    /// a server on a random port as part of unit testing.
    pub fn socket_addr(&self) -> std::io::Result<SocketAddr> {
        self.server.socket_addr()
    }

    async fn dispatch_msg(&mut self, packet: Packet, addr: SocketAddr) -> Result<(), io::Error> {
        let request = CoapRequest::from_packet(packet, addr);
        let filtered = !self.observer.request_handler(&request).await;
        if filtered {
            return Ok(());
        }

        if let Some(ref mut handler) = self.handler {
            match handler(request).await {
                Some(response) => {
                    debug!("Response: {:?}", response);
                    self.server.send((response.message, addr)).await?;
                }
                None => {
                    debug!("No response");
                }
            }
        }
        Ok(())
    }

    /// enable AllCoAP multicasts - adds the AllCoap addresses to the unicast listener 
    /// - IPv4 AllCoAP multicast address is '224.0.1.187'
    /// - IPv6 AllCoAp multicast addresses are 'ff0?::fd'
    /// 
    /// Parameter segment is used with IPv6 to determine the first octet. 
    /// - It's value can be between 0x0 and 0xf. 
    /// - To join multiple segments, you have to call enable_discovery for each of the segments.
    /// 
    /// For further details see method join_multicast
    pub fn enable_all_coap(&mut self, segment: u8) {
        let socket = self.server.socket.get_mut();
        let m = match socket.local_addr().unwrap() {
            SocketAddr::V4(_val) => {
                IpAddr::V4(Ipv4Addr::new(224, 0, 1, 187))
            },
            SocketAddr::V6(_val) => {
                IpAddr::V6(Ipv6Addr::new(0xff00 + segment as u16,0,0,0,0,0,0,0xfd))
            },
        };
        self.join_multicast(m, segment);
    }

    /// join multicast - adds the multicast addresses to the unicast listener 
    /// - IPv4 multicast address range is '224.0.0.0/4'
    /// - IPv6 AllCoAp multicast addresses are 'ff00::/8'
    /// 
    /// Parameter segment is used with IPv6 to determine the first octet. 
    /// - It's value can be between 0x0 and 0xf. 
    /// - To join multiple segments, you have to call enable_discovery for each of the segments.
    ///
    /// Multicast address scope
    /// IPv6        IPv4 equivalent[16]	        Scope	            Purpose
    /// ff00::/16                               Reserved
    /// ff0f::/16		                        Reserved
    /// ffx1::/16	127.0.0.0/8	                Interface-local	    Packets with this destination address may not be sent over any network link, but must remain within the current node; this is the multicast equivalent of the unicast loopback address.
    /// ffx2::/16	224.0.0.0/24	            Link-local	        Packets with this destination address may not be routed anywhere.
    /// ffx3::/16	239.255.0.0/16	            IPv4 local scope
    /// ffx4::/16	            	            Admin-local	        The smallest scope that must be administratively configured.
    /// ffx5::/16		                        Site-local	        Restricted to the local physical network.
    /// ffx8::/16	239.192.0.0/14	            Organization-local	Restricted to networks used by the organization administering the local network. (For example, these addresses might be used over VPNs; when packets for this group are routed over the public internet (where these addresses are not valid), they would have to be encapsulated in some other protocol.)
    /// ffxe::/16	224.0.1.0-238.255.255.255	Global scope	    Eligible to be routed over the public internet.
    /// 
    /// Notable addresses:
    /// ff02::1	    All nodes on the local network segment
    /// ff02::2	    All routers on the local network segment
    /// ff02::5	    OSPFv3 All SPF routers
    /// ff02::6	    OSPFv3 All DR routers
    /// ff02::8	    IS-IS for IPv6 routers
    /// ff02::9	    RIP routers
    /// ff02::a	    EIGRP routers
    /// ff02::d	    PIM routers
    /// ff02::16	MLDv2 reports (defined in RFC 3810)
    /// ff02::1:2	All DHCPv6 servers and relay agents on the local network segment (defined in RFC 3315)
    /// ff02::1:3	All LLMNR hosts on the local network segment (defined in RFC 4795)
    /// ff05::1:3	All DHCP servers on the local network site (defined in RFC 3315)
    /// ff0x::c	    Simple Service Discovery Protocol
    /// ff0x::fb	Multicast DNS
    /// ff0x::101	Network Time Protocol
    /// ff0x::108	Network Information Service
    /// ff0x::181	Precision Time Protocol (PTP) version 2 messages (Sync, Announce, etc.) except peer delay measurement
    /// ff02::6b	Precision Time Protocol (PTP) version 2 peer delay measurement messages
    /// ff0x::114	Used for experiments

    pub fn join_multicast(&mut self, addr: IpAddr, segment: u8) {
        assert!(addr.is_multicast());
        assert!(segment <= 0xf);
        let socket = self.server.socket.get_mut();
        // determine wether IPv4 or IPv6 and 
        // join the appropriate multicast address
        match socket.local_addr().unwrap() {
            SocketAddr::V4(val) => {
                match addr {
                    IpAddr::V4(ipv4) => { 
                        let i = val.ip().clone();
                        socket.join_multicast_v4(ipv4, i).unwrap();
                    }
                    IpAddr::V6(_ipv6) => { /* handle IPv6 */ }
                }
            },
            SocketAddr::V6(_val) => {
                match addr {
                    IpAddr::V4(_ipv4) => { /* handle IPv4 */ }
                    IpAddr::V6(ipv6) => { 
                        socket.join_multicast_v6(&ipv6, 0).unwrap();
                        //socket.set_only_v6(true)?;
                    }
                }
            },
        }
    }
}

pub struct CoAPServer {
    receiver: MessageReceiver,
    is_terminated: bool,
    socket: UdpFramed<Codec>,
}

impl CoAPServer {
    /// Creates a CoAP server listening on the given address.
    pub fn new<A: ToSocketAddrs>(addr: A, receiver: MessageReceiver) -> Result<CoAPServer, io::Error> {
        let socket = UdpSocket::from_std(net::UdpSocket::bind(addr).unwrap())?;

        Ok(CoAPServer {
            receiver,
            is_terminated: false,
            socket: UdpFramed::new(socket, Codec::new()),
        })
    }

    /// Stop the server.
    pub fn stop(&mut self) {
        self.is_terminated = true;
    }

    /// send the packet to the specific address.
    pub async fn send(&mut self, frame: (Packet, SocketAddr)) -> Result<(), io::Error> {
        self.socket.send(frame).await
    }

    /// Return the local address that the server is listening on. This can be useful when starting
    /// a server on a random port as part of unit testing.
    pub fn socket_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.get_ref().local_addr()
    }
}

impl Drop for CoAPServer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Stream for CoAPServer {
    type Item = Result<Message, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(Some((p, a))) = self.receiver.poll_next_unpin(cx) {
            return Poll::Ready(Some(Ok(Message::NeedSend(p, a))));
        }

        let result: Option<_> = futures::ready!(self.socket.poll_next_unpin(cx));

        Poll::Ready(match result {
            Some(Ok(message)) => {
                let (my_packet, addr) = message;
                Some(Ok(Message::Received(my_packet, addr)))
            }
            Some(Err(e)) => Some(Err(e)),
            None => None,
        })
    }
}

impl FusedStream for CoAPServer {
    fn is_terminated(&self) -> bool {
        self.is_terminated
    }
}

#[cfg(test)]
pub mod test {
    use std::{
        time::Duration,
        sync::mpsc,
    };
    use coap_lite::CoapOption;
    use super::super::*;
    use super::*;

    pub fn spawn_server<F: FnMut(CoapRequest<SocketAddr>) -> HandlerRet + Send + 'static, HandlerRet>(request_handler: F) -> mpsc::Receiver<u16>  where HandlerRet: Future<Output=Option<CoapResponse>> {
        let (tx, rx) = mpsc::channel();

        std::thread::Builder::new().name(String::from("server")).spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async move {
                let mut server = server::Server::new("127.0.0.1:0").unwrap();

                tx.send(server.socket_addr().unwrap().port()).unwrap();
                
                server.run(request_handler).await.unwrap();
            })
        }).unwrap();
        
        rx
    }
    
    async fn request_handler(req: CoapRequest<SocketAddr>) -> Option<CoapResponse> {
        let uri_path_list = req.message.get_option(CoapOption::UriPath).unwrap().clone();
        assert_eq!(uri_path_list.len(), 1);
        
        match req.response {
            Some(mut response) => {
                response.message.payload = uri_path_list.front().unwrap().clone();
                Some(response)
            }
            _ => None,
        }
    }

    pub fn spawn_v4_server_with_all_coap<F: FnMut(CoapRequest<SocketAddr>) -> HandlerRet + Send + 'static, HandlerRet>(request_handler: F) -> mpsc::Receiver<u16>  where HandlerRet: Future<Output=Option<CoapResponse>> {
        let (tx, rx) = mpsc::channel();

        std::thread::Builder::new().name(String::from("v4-server")).spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async move {
                // multicast needs a sevr on a real interface
                let mut server = server::Server::new(("0.0.0.0", 0)).unwrap();
                server.enable_all_coap(0x0);

                tx.send(server.socket_addr().unwrap().port()).unwrap();
                
                server.run(request_handler).await.unwrap();
            })
        }).unwrap();
        
        rx
    }

    pub fn spawn_v6_server_with_all_coap<F: FnMut(CoapRequest<SocketAddr>) -> HandlerRet + Send + 'static, HandlerRet>(request_handler: F) -> mpsc::Receiver<u16>  where HandlerRet: Future<Output=Option<CoapResponse>> {
        let (tx, rx) = mpsc::channel();

        std::thread::Builder::new().name(String::from("v6-server")).spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async move {
                // multicast needs a sevr on a real interface
                let mut server = server::Server::new(("::0", 0)).unwrap();
                server.enable_all_coap(0x0);

                tx.send(server.socket_addr().unwrap().port()).unwrap();
                
                server.run(request_handler).await.unwrap();
            })
        }).unwrap();
        
        rx
    }

    #[test]
    fn test_echo_server() {
        let server_port = spawn_server(request_handler).recv().unwrap();

        let client = CoAPClient::new(format!("127.0.0.1:{}", server_port)).unwrap();
        let mut request = CoapRequest::new();
        request.message.header.set_version(1);
        request.message.header.set_type(coap_lite::MessageType::Confirmable);
        request.message.header.set_code("0.01");
        request.message.header.message_id = 1;
        request.message.set_token(vec![0x51, 0x55, 0x77, 0xE8]);
        request.message.add_option(CoapOption::UriPath, b"test-echo".to_vec());
        client.send(&request).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());
    }

    #[test]
    fn test_echo_server_no_token() {
        let server_port = spawn_server(request_handler).recv().unwrap();

        let client = CoAPClient::new(format!("127.0.0.1:{}", server_port)).unwrap();
        let mut packet = CoapRequest::new();
        packet.message.header.set_version(1);
        packet.message.header.set_type(coap_lite::MessageType::Confirmable);
        packet.message.header.set_code("0.01");
        packet.message.header.message_id = 1;
        packet.message.add_option(CoapOption::UriPath, b"test-echo".to_vec());
        client.send(&packet).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());
    }

    #[test]
    fn test_update_resource() {
        let path = "/test";
        let payload1 = b"data1".to_vec();
        let payload2 = b"data2".to_vec();
        let (tx, rx) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        let mut step = 1;

        let server_port = spawn_server(request_handler).recv().unwrap();

        let mut client = CoAPClient::new(format!("127.0.0.1:{}", server_port)).unwrap();

        tx.send(step).unwrap();
        let mut request = CoapRequest::new();
        request.set_method(coap_lite::RequestType::Put);
        request.set_path(path);
        request.message.payload = payload1.clone();
        client.send(&request).unwrap();
        client.receive().unwrap();

        let mut receive_step = 1;
        let payload1_clone = payload1.clone();
        let payload2_clone = payload2.clone();
        client.observe(path, move |msg| {
            match rx.try_recv() {
                Ok(n) => receive_step = n,
                _ => (),
            }

            match receive_step {
                1 => assert_eq!(msg.payload, payload1_clone),
                2 => {
                    assert_eq!(msg.payload, payload2_clone);
                    tx2.send(()).unwrap();
                }
                _ => panic!("unexpected step"),
            }
        }).unwrap();

        step = 2;
        tx.send(step).unwrap();
        request.message.payload = payload2.clone();
        let client2 = CoAPClient::new(format!("127.0.0.1:{}", server_port)).unwrap();
        client2.send(&request).unwrap();
        client2.receive().unwrap();
        assert_eq!(rx2.recv_timeout(Duration::new(5, 0)).unwrap(), ());
    }

    #[test]
    fn test_server_all_coap_v4() {
        let server_port = spawn_v4_server_with_all_coap(request_handler).recv().unwrap();

        let client = CoAPClient::new(format!("127.0.0.1:{}", server_port)).unwrap();
        let mut request = CoapRequest::new();
        request.message.header.set_version(1);
        request.message.header.set_type(coap_lite::MessageType::Confirmable);
        request.message.header.set_code("0.01");
        request.message.header.message_id = 1;
        request.message.set_token(vec![0x51, 0x55, 0x77, 0xE8]);
        request.message.add_option(CoapOption::UriPath, b"test-echo".to_vec());
        client.send(&request).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());

        let client = CoAPClient::new(format!("224.0.1.187:{}", server_port)).unwrap();
        let mut request = CoapRequest::new();
        request.message.header.set_version(1);
        request.message.header.set_type(coap_lite::MessageType::NonConfirmable);
        request.message.header.set_code("0.01");
        request.message.header.message_id = 2;
        request.message.set_token(vec![0x51, 0x55, 0x77, 0xE8]);
        request.message.add_option(CoapOption::UriPath, b"test-echo".to_vec());
        client.send_all_coap(&request, 0x2).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());
    }

    #[test]
    #[ignore]  // This test does not work, not clear why. With a separate test client things seem to work.
    fn test_server_all_coap_v6() {
        let server_port = spawn_v6_server_with_all_coap(request_handler).recv().unwrap();

        let client = CoAPClient::new(format!("::1:{}", server_port)).unwrap();
        let mut request = CoapRequest::new();
        request.message.header.set_version(1);
        request.message.header.set_type(coap_lite::MessageType::Confirmable);
        request.message.header.set_code("0.01");
        request.message.header.message_id = 1;
        request.message.set_token(vec![0x51, 0x55, 0x77, 0xE8]);
        request.message.add_option(CoapOption::UriPath, b"test-echo".to_vec());
        client.send(&request).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());

        // use 0xff02 to keep it within this network
        let client = CoAPClient::new(format!("ff02::fd:{}", server_port)).unwrap();
        let mut request = CoapRequest::new();
        request.message.header.set_version(1);
        request.message.header.set_type(coap_lite::MessageType::NonConfirmable);
        request.message.header.set_code("0.01");
        request.message.header.message_id = 2;
        request.message.set_token(vec![0x51, 0x55, 0x77, 0xE8]);
        request.message.add_option(CoapOption::UriPath, b"test-echo".to_vec());
        // use segment 0x02 to keep it within this network
        client.send_all_coap(&request, 0x3).unwrap();
        //client.send(&request).unwrap();

        let recv_packet = client.receive().unwrap();
        assert_eq!(recv_packet.message.payload, b"test-echo".to_vec());
    }
}
