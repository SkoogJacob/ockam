#[cfg(test)]
mod test {
    use bytes::{Buf, BufMut, BytesMut};
    use indexmap::IndexMap;
    use kafka_protocol::messages::produce_request::{PartitionProduceData, TopicProduceData};
    use kafka_protocol::messages::{
        fetch_request::{FetchPartition, FetchTopic},
        fetch_response::FetchableTopicResponse,
        fetch_response::PartitionData,
        ApiKey, BrokerId, FetchRequest, FetchResponse, ProduceRequest, RequestHeader,
        ResponseHeader, TopicName,
    };
    use kafka_protocol::protocol::Builder;
    use kafka_protocol::protocol::Decodable as KafkaDecodable;
    use kafka_protocol::protocol::Encodable as KafkaEncodable;
    use kafka_protocol::protocol::StrBytes;
    use kafka_protocol::records::Record;
    use kafka_protocol::records::{
        Compression, RecordBatchDecoder, RecordBatchEncoder, RecordEncodeOptions, TimestampType,
    };
    use minicbor::Decoder;
    use ockam::compat::tokio::io::DuplexStream;
    use ockam::Context;
    use ockam_core::api::Request;
    use ockam_core::api::{Response, Status};
    use ockam_core::errcode::{Kind, Origin};
    use ockam_core::{route, Error};
    use ockam_multiaddr::MultiAddr;
    use ockam_node::compat::tokio;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::net::TcpStream;
    use tokio::sync::Mutex;
    use tokio::task::JoinHandle;
    use uuid::Uuid;

    use crate::nodes::models::portal::CreateOutlet;
    use crate::nodes::models::services::{
        StartKafkaConsumerRequest, StartKafkaProducerRequest, StartServiceRequest,
    };
    use crate::nodes::NODEMANAGER_ADDR;

    const TEST_KAFKA_API_VERSION: i16 = 12;

    async fn send_and_receive_ignore_body(
        context: &mut Context,
        request: Vec<u8>,
    ) -> ockam::Result<()> {
        let buffer: Vec<u8> = context
            .send_and_receive(route![NODEMANAGER_ADDR], request)
            .await?;

        let mut decoder = Decoder::new(&buffer);
        let response: Response = decoder.decode()?;
        match response.status() {
            None => {
                return Err(Error::new(
                    Origin::Transport,
                    Kind::Invalid,
                    "missing status",
                ))
            }
            Some(Status::Ok) => {}
            Some(status) => {
                return Err(Error::new(
                    Origin::Transport,
                    Kind::Invalid,
                    format!("{}", status),
                ))
            }
        }

        Ok(())
    }

    struct TcpServerSimulator {
        stream: DuplexStream,
        join_handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
        is_stopping: Arc<AtomicBool>,
    }

    impl TcpServerSimulator {
        pub fn stream(&mut self) -> &mut DuplexStream {
            &mut self.stream
        }

        ///stops every async task running and wait for completion
        /// must be called to avoid leaks to be sure everything is closed before
        /// moving on the next test
        pub async fn destroy_and_wait(self) -> () {
            self.is_stopping.store(true, Ordering::SeqCst);
            //we want to close the channel _before_ joining current handles to interrupt them
            drop(self.stream);
            let mut guard = self.join_handles.lock().await;
            for handle in guard.iter_mut() {
                //we don't care about failures
                let _ = handle.await;
            }
        }

        ///starts a tcp listener for one connection and returns a virtual buffer
        /// linked to the first socket
        pub async fn start(address: &str) -> Self {
            let listener = TcpListener::bind(address).await.unwrap();
            let join_handles: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
            let is_stopping = Arc::new(AtomicBool::new(false));

            let (test_side_duplex, simulator_side_duplex) = tokio::io::duplex(4096);
            let (simulator_read_half, simulator_write_half) =
                tokio::io::split(simulator_side_duplex);

            let handle: JoinHandle<()> = {
                let is_stopping = is_stopping.clone();
                let join_handles = join_handles.clone();
                tokio::spawn(async move {
                    let socket;
                    loop {
                        //tokio would block on the listener forever, we need to poll a little in
                        //order to interrupt it
                        let timeout_future =
                            tokio::time::timeout(Duration::from_millis(200), listener.accept());
                        if let Ok(result) = timeout_future.await {
                            match result {
                                Ok((current_socket, _)) => {
                                    socket = current_socket;
                                    break;
                                }
                                Err(_) => {
                                    return;
                                }
                            }
                        }
                        if is_stopping.load(Ordering::SeqCst) {
                            return;
                        }
                    }

                    let (socket_read_half, socket_write_half) = socket.into_split();
                    let handle: JoinHandle<()> = tokio::spawn(async move {
                        Self::relay_traffic(
                            "socket_read_half",
                            socket_read_half,
                            "simulator_write_half",
                            simulator_write_half,
                        )
                        .await
                    });
                    join_handles.lock().await.push(handle);

                    let handle: JoinHandle<()> = tokio::spawn(async move {
                        Self::relay_traffic(
                            "simulator_read_half",
                            simulator_read_half,
                            "socket_write_half",
                            socket_write_half,
                        )
                        .await
                    });
                    join_handles.lock().await.push(handle);
                })
            };
            join_handles.lock().await.push(handle);

            Self {
                stream: test_side_duplex,
                join_handles,
                is_stopping,
            }
        }

        async fn relay_traffic<W: AsyncWriteExt + Unpin, R: AsyncReadExt + Unpin>(
            read_half_name: &'static str,
            mut read_half: R,
            write_half_name: &'static str,
            mut write_half: W,
        ) {
            let mut buffer = [0; 1024];
            loop {
                let read = match read_half.read(&mut buffer).await {
                    Ok(read) => read,
                    Err(err) => {
                        warn!("{write_half_name} error: closing channel: {:?}", err);
                        break;
                    }
                };

                if read == 0 {
                    info!("{read_half_name} returned empty buffer: clean channel close");
                    break;
                }
                if write_half.write(&buffer[0..read]).await.is_err() {
                    warn!("{write_half_name} error: closing channel");
                    break;
                }
            }
        }
    }

    #[allow(non_snake_case)]
    #[ockam_macros::test(timeout = 5_000)]
    async fn producer__flow_with_mock_kafka__content_encryption_and_decryption(
        context: &mut Context,
    ) -> ockam::Result<()> {
        let _handler = crate::util::test::start_manager_for_tests(context).await?;

        //ask the node manager to create needed services
        {
            //for both consumer and producer we create a mock kafka server so we can read
            //and validate traffic as well as reply
            let request =
                Request::post("/node/services/kafka_consumer").body(StartServiceRequest::new(
                    StartKafkaConsumerRequest::new(
                        "127.0.1.1".parse().unwrap(),
                        4000,
                        (4001, 4001),
                        MultiAddr::try_from("/service/kafka_consumer_outlet")?,
                    ),
                    "kafka_consumer_listener",
                ));
            send_and_receive_ignore_body(context, request.to_vec()?).await?;

            let request = Request::post("/node/outlet").body(CreateOutlet::new(
                "127.0.1.1:5000",
                "kafka_consumer_outlet",
                None,
                None,
            ));
            send_and_receive_ignore_body(context, request.to_vec()?).await?;

            let request =
                Request::post("/node/services/kafka_producer").body(StartServiceRequest::new(
                    StartKafkaProducerRequest::new(
                        "127.0.1.1".parse().unwrap(),
                        6000,
                        (6001, 6001),
                        MultiAddr::try_from("/service/kafka_producer_outlet")?,
                    ),
                    "kafka_producer_listener",
                ));
            send_and_receive_ignore_body(context, request.to_vec()?).await?;

            let request = Request::post("/node/outlet").body(CreateOutlet::new(
                "127.0.1.1:7000",
                "kafka_producer_outlet",
                None,
                None,
            ));
            send_and_receive_ignore_body(context, request.to_vec()?).await?;
        }

        let mut consumer_mock_kafka = TcpServerSimulator::start("127.0.1.1:5000").await;
        //to create forwarder for the partition 1 of 'my-topic'
        simulate_first_kafka_consumer_empty_reply_and_ignore_result(&mut consumer_mock_kafka).await;
        drop(consumer_mock_kafka);

        let mut producer_mock_kafka = TcpServerSimulator::start("127.0.1.1:7000").await;
        let request = simulate_kafka_producer_and_read_request(&mut producer_mock_kafka).await;

        let encrypted_body = request
            .topic_data
            .iter()
            .next()
            .as_ref()
            .unwrap()
            .1
            .partition_data
            .get(0)
            .unwrap()
            .records
            .as_ref()
            .unwrap();

        let mut encrypted_body = BytesMut::from(encrypted_body.as_ref());
        let records = RecordBatchDecoder::decode(&mut encrypted_body).unwrap();

        assert_ne!(
            records.get(0).unwrap().value.as_ref().unwrap(),
            "hello world!".as_bytes()
        );

        let mut consumer_mock_kafka = TcpServerSimulator::start("127.0.1.1:5000").await;
        let plain_fetch_response =
            simulate_kafka_consumer_and_read_response(&mut consumer_mock_kafka, &request).await;

        let plain_content = plain_fetch_response
            .responses
            .get(0)
            .as_ref()
            .unwrap()
            .partitions
            .get(0)
            .as_ref()
            .unwrap()
            .records
            .as_ref()
            .unwrap();

        let mut plain_content = BytesMut::from(plain_content.as_ref());
        let records = RecordBatchDecoder::decode(&mut plain_content).unwrap();

        assert_eq!(
            records.get(0).as_ref().unwrap().value.as_ref().unwrap(),
            "hello world!".as_bytes()
        );

        context.stop().await?;
        consumer_mock_kafka.destroy_and_wait().await;
        producer_mock_kafka.destroy_and_wait().await;
        Ok(())
    }

    async fn simulate_kafka_producer_and_read_request(
        producer_mock_kafka: &mut TcpServerSimulator,
    ) -> ProduceRequest {
        let mut kafka_client_connection = TcpStream::connect("127.0.1.1:6000").await.unwrap();
        send_kafka_produce_request(&mut kafka_client_connection).await;
        read_kafka_message::<&mut DuplexStream, RequestHeader, ProduceRequest>(
            producer_mock_kafka.stream(),
        )
        .await
    }

    async fn send_kafka_produce_request(stream: &mut TcpStream) {
        let header = RequestHeader::builder()
            .request_api_key(ApiKey::ProduceKey as i16)
            .request_api_version(TEST_KAFKA_API_VERSION)
            .correlation_id(1)
            .client_id(Some(StrBytes::from_str("my-client-id")))
            .unknown_tagged_fields(Default::default())
            .build()
            .unwrap();

        let mut encoded = BytesMut::new();
        RecordBatchEncoder::encode(
            &mut encoded,
            vec![Record {
                transactional: false,
                control: false,
                partition_leader_epoch: 0,
                producer_id: 0,
                producer_epoch: 0,
                timestamp_type: TimestampType::Creation,
                offset: 0,
                sequence: 0,
                timestamp: 0,
                key: None,
                value: Some(BytesMut::from("hello world!").freeze()),
                headers: Default::default(),
            }]
            .iter(),
            &RecordEncodeOptions {
                version: 2,
                compression: Compression::None,
            },
        )
        .unwrap();

        let mut topic_data = IndexMap::new();
        topic_data.insert(
            TopicName::from(StrBytes::from_str("my-topic-name")),
            TopicProduceData::builder()
                .partition_data(vec![PartitionProduceData::builder()
                    .index(1)
                    .records(Some(encoded.freeze()))
                    .unknown_tagged_fields(Default::default())
                    .build()
                    .unwrap()])
                .unknown_tagged_fields(Default::default())
                .build()
                .unwrap(),
        );
        let request = ProduceRequest::builder()
            .transactional_id(None)
            .acks(0)
            .timeout_ms(0)
            .topic_data(topic_data)
            .unknown_tagged_fields(Default::default())
            .build()
            .unwrap();

        send_kafka_message(stream, header, request).await;
    }

    //this is needed in order to make the consumer create the forwarders to the secure
    //channel
    async fn simulate_first_kafka_consumer_empty_reply_and_ignore_result(
        mock_kafka_connection: &mut TcpServerSimulator,
    ) -> () {
        let mut kafka_client_connection = TcpStream::connect("127.0.1.1:4000").await.unwrap();
        send_kafka_fetch_request(&mut kafka_client_connection).await;
        //we don't want the answer, but we need to be sure the
        // message passed through and the forwarder had been created
        mock_kafka_connection
            .stream
            .read_exact(&mut [0; 4])
            .await
            .unwrap();
    }

    //we use the encrypted producer request to generate the encrypted fetch response
    async fn simulate_kafka_consumer_and_read_response(
        mock_kafka_connection: &mut TcpServerSimulator,
        producer_request: &ProduceRequest,
    ) -> FetchResponse {
        let mut kafka_client_connection = TcpStream::connect("127.0.1.1:4000").await.unwrap();
        send_kafka_fetch_request(&mut kafka_client_connection).await;
        let _fetch_request: FetchRequest =
            read_kafka_message::<&mut DuplexStream, RequestHeader, FetchRequest>(
                mock_kafka_connection.stream(),
            )
            .await;

        send_kafka_fetch_response(mock_kafka_connection.stream(), &producer_request).await;
        read_kafka_message::<&mut TcpStream, ResponseHeader, FetchResponse>(
            &mut kafka_client_connection,
        )
        .await
    }

    async fn send_kafka_fetch_response<S: AsyncWriteExt + Unpin>(
        stream: S,
        producer_request: &ProduceRequest,
    ) {
        let topic_name = TopicName::from(StrBytes::from_str("my-topic-name"));
        let producer_content = producer_request
            .topic_data
            .get(&topic_name)
            .unwrap()
            .partition_data
            .get(0)
            .unwrap()
            .records
            .clone();

        send_kafka_message(
            stream,
            ResponseHeader::builder()
                .correlation_id(1)
                .unknown_tagged_fields(Default::default())
                .build()
                .unwrap(),
            FetchResponse::builder()
                .throttle_time_ms(Default::default())
                .error_code(Default::default())
                .session_id(Default::default())
                .responses(vec![FetchableTopicResponse::builder()
                    .topic(topic_name)
                    .topic_id(Default::default())
                    .partitions(vec![PartitionData::builder()
                        .partition_index(1)
                        .error_code(Default::default())
                        .high_watermark(Default::default())
                        .last_stable_offset(Default::default())
                        .log_start_offset(Default::default())
                        .diverging_epoch(Default::default())
                        .current_leader(Default::default())
                        .snapshot_id(Default::default())
                        .aborted_transactions(Default::default())
                        .preferred_read_replica(Default::default())
                        .records(producer_content)
                        .unknown_tagged_fields(Default::default())
                        .build()
                        .unwrap()])
                    .unknown_tagged_fields(Default::default())
                    .build()
                    .unwrap()])
                .unknown_tagged_fields(Default::default())
                .build()
                .unwrap(),
        )
        .await;
    }

    async fn send_kafka_fetch_request(stream: &mut TcpStream) {
        send_kafka_message(
            stream,
            RequestHeader::builder()
                .request_api_key(ApiKey::FetchKey as i16)
                .request_api_version(TEST_KAFKA_API_VERSION)
                .correlation_id(1)
                .client_id(Some(StrBytes::from_str("my-client-id")))
                .unknown_tagged_fields(Default::default())
                .build()
                .unwrap(),
            FetchRequest::builder()
                .cluster_id(None)
                .replica_id(BrokerId::default())
                .max_wait_ms(0)
                .min_bytes(0)
                .max_bytes(0)
                .isolation_level(0)
                .session_id(0)
                .session_epoch(0)
                .topics(vec![FetchTopic::builder()
                    .topic(TopicName::from(StrBytes::from_str("my-topic-name")))
                    .topic_id(Uuid::from_slice(b"my-topic-name___").unwrap())
                    .partitions(vec![FetchPartition::builder()
                        .partition(1)
                        .current_leader_epoch(0)
                        .fetch_offset(0)
                        .last_fetched_epoch(0)
                        .log_start_offset(0)
                        .partition_max_bytes(0)
                        .unknown_tagged_fields(Default::default())
                        .build()
                        .unwrap()])
                    .unknown_tagged_fields(Default::default())
                    .build()
                    .unwrap()])
                .forgotten_topics_data(Default::default())
                .rack_id(Default::default())
                .unknown_tagged_fields(Default::default())
                .build()
                .unwrap(),
        )
        .await;
    }

    async fn send_kafka_message<S: AsyncWriteExt + Unpin, H: KafkaEncodable, T: KafkaEncodable>(
        mut stream: S,
        header: H,
        message: T,
    ) {
        let size = header.compute_size(TEST_KAFKA_API_VERSION).unwrap()
            + message.compute_size(TEST_KAFKA_API_VERSION).unwrap();

        let mut request_buffer = BytesMut::new();
        request_buffer.put_u32(size as u32);

        header
            .encode(&mut request_buffer, TEST_KAFKA_API_VERSION)
            .unwrap();
        message
            .encode(&mut request_buffer, TEST_KAFKA_API_VERSION)
            .unwrap();

        trace!("send_kafka_message...");
        stream.write_all(&request_buffer).await.unwrap();
        stream.flush().await.unwrap();
        trace!("send_kafka_message...done");
    }

    async fn read_kafka_message<S: AsyncReadExt + Unpin, H: KafkaDecodable, T: KafkaDecodable>(
        mut stream: S,
    ) -> T {
        trace!("read_kafka_message...");
        let size = {
            let mut length_buffer = [0; 4];
            let read = stream.read_exact(&mut length_buffer).await.unwrap();

            assert_eq!(4, read);
            BytesMut::from(length_buffer.as_slice()).get_u32()
        };
        info!("incoming message size: {size}");

        let mut header_and_request_buffer = [0; 1024];
        let read = stream
            .read_exact(&mut header_and_request_buffer[0..size as usize])
            .await
            .unwrap();
        assert_eq!(size as usize, read);

        let mut header_and_request_buffer = BytesMut::from(header_and_request_buffer.as_slice());

        let _header = H::decode(&mut header_and_request_buffer, TEST_KAFKA_API_VERSION).unwrap();
        let request = T::decode(&mut header_and_request_buffer, TEST_KAFKA_API_VERSION).unwrap();
        trace!("read_kafka_message...done");
        request
    }
}
