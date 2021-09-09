mod codec;

use log::{error, info, warn};

use proxy_wasm::hostcalls::{define_metric, increment_metric};
use proxy_wasm::types::{Action, LogLevel, MetricType, PeerType};
use proxy_wasm::{
    self,
    traits::{Context, StreamContext},
};

use mongo_protocol::{MongoMessage, MsgHeader};

use codec::MongoProtocolDecoder;

struct MongoDbFilter {
    context_id: u32,
    root_context_id: u32,
    decoder: MongoProtocolDecoder,
    filter_active: bool,
    counter: u32,
}

impl Context for MongoDbFilter {}

#[no_mangle]
pub fn _start() {
    proxy_wasm::set_log_level(LogLevel::Trace);
    proxy_wasm::set_stream_context(|context_id, root_context_id| -> Box<dyn StreamContext> {
        println!("_start for context {}", context_id);

        // Envoy defines the following tag extraction patterns for MongoDb. We could roll our
        // own, or possibly use these to bootstrap:
        //
        // mongo.[<stat_prefix>.]collection.[<collection>.]callsite.(<callsite>.)query.<base_stat>
        // mongo.[<stat_prefix>.]collection.(<collection>.)query.<base_stat>
        // mongo.[<stat_prefix>.]cmd.(<cmd>.)<base_stat>
        // mongo.(<stat_prefix>.)*

        let counter = define_metric(
            MetricType::Counter,
            &format!("mongo.mongoproxy.total_queries"),
        )
        .unwrap();

        Box::new(MongoDbFilter {
            context_id,
            root_context_id,
            decoder: MongoProtocolDecoder::new(),
            filter_active: true,
            counter,
        })
    });
}

impl MongoDbFilter {
    fn get_messages(&mut self, data: Vec<u8>) -> Vec<(MsgHeader, MongoMessage)> {
        match self.decoder.decode_messages(&data) {
            Ok(message_list) => message_list,
            Err(e) => {
                println!("Unable to decode Mongo protocol: {}\nStopping.", e);
                self.filter_active = false;
                vec![]
            }
        }
    }
}

impl StreamContext for MongoDbFilter {
    fn on_new_connection(&mut self) -> Action {
        println!(
            "ctx {}: new connection: root={}",
            self.context_id, self.root_context_id
        );
        Action::Continue
    }

    // When we receive something from the "client"
    fn on_downstream_data(&mut self, data_size: usize, _end_of_stream: bool) -> Action {
        if let Some(data) = self.get_downstream_data(0, data_size) {
            for (hdr, msg) in self.get_messages(data) {
                //println!("From downstream:\nhdr: {:?}\nmsg: {:?}\n", hdr, msg);
                if let Err(e) = increment_metric(self.counter, 1) {
                    println!("Metric inc error for {}: {:?}", self.counter, e);
                }
            }
        } else {
            println!("ctx {}: no data :(", self.context_id);
        }

        Action::Continue
    }

    fn on_downstream_close(&mut self, _peer_type: PeerType) {
        println!("ctx {}: Downstream closed", self.context_id);
    }

    // When we receive something from the "server"
    fn on_upstream_data(&mut self, data_size: usize, _end_of_stream: bool) -> Action {
        if let Some(data) = self.get_upstream_data(0, data_size) {
            for (hdr, msg) in self.get_messages(data) {
                //println!("From upstream:\nhdr: {:?}\nmsg: {:?}\n", hdr, msg);
            }
        } else {
            println!("ctx {}: no data :(", self.context_id);
        }
        Action::Continue
    }

    fn on_upstream_close(&mut self, _peer_type: PeerType) {
        println!("ctx {}: Upstream connection closed", self.context_id);
    }

    fn on_log(&mut self) {
        println!("ctx {}: on_log called", self.context_id);
    }
}
