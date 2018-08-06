use admin;
use config::{RustProxyConfig, BackendPoolConfig, load_config};
use backendpool;
use backendpool::BackendPool;
use mio::*;
use mio::unix::{UnixReady};
use std::collections::*;
use std::io::{Write};
use std::mem;
use std::time::{Instant, Duration};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

// For admin reqs.
use backend::parse_redis_command;
use toml;
use std::process;

pub const NULL_TOKEN: Token = Token(0);
pub const SERVER: Token = Token(1);

const FIRST_SOCKET_INDEX: usize = 10;
pub const SOCKET_INDEX_SHIFT: usize = 2;

pub type TimeoutQueue = VecDeque<(Instant, Token)>;
pub type BackendToken = Token;
pub type PoolToken = Token;
pub type ClientToken = Token;

pub enum StreamType {
    AdminClient,
    PoolClient,
    PoolServer,
}

#[derive(Clone, Copy, Debug)]
pub enum Subscriber {
    Timeout(Token),
    PoolServer(Token),
    PoolListener,
    PoolClient(Token),
    AdminListener,
    AdminClient,
}

pub fn generate_backend_token(
    next_socket_index: &Cell<usize>,
    backend_tokens: &RefCell<HashMap<BackendToken, PoolToken>>,
    pool_token: PoolToken
) -> BackendToken {
    next_socket_index.set(next_socket_index.get() + SOCKET_INDEX_SHIFT);
    let new_token = Token(next_socket_index.get());
    backend_tokens.borrow_mut().insert(new_token, pool_token);
    return new_token.clone();
}

pub fn generate_client_token(next_socket_index: &Cell<usize>) -> ClientToken {
    next_socket_index.set(next_socket_index.get() + SOCKET_INDEX_SHIFT);
    let new_token = Token(next_socket_index.get());
    new_token
}

// High-level struct that contains everything for a rustproxy instance.
pub struct RustProxy {
    // This may just get integrated back into RustProxy.
    pub admin: admin::AdminPort,

    // Configs
    pub config: RustProxyConfig,
    pub staged_config: Option<RustProxyConfig>,

    // Child structs.
    pub backendpools: HashMap<PoolToken, BackendPool>,

    // Registry...
    backend_configs: HashMap<BackendPoolConfig, PoolToken>,
    backend_tokens: Rc<RefCell<HashMap<Token, PoolToken>>>,
    subscribers: HashMap<Token, Subscriber>,
    pub written_sockets: Box<VecDeque<(Token, StreamType)>>,
    poll: Poll,
    next_socket_index: Rc<Cell<usize>>,
    // A queue of messages/events. Used to determine the next event to wait for.
    timeout_queue : TimeoutQueue,
}
impl RustProxy {
    pub fn new(config_path: String) -> Result<RustProxy, String> {
        let config = try!(load_config(config_path));
        let next_socket_index = Rc::new(Cell::new(FIRST_SOCKET_INDEX));
        let poll = match Poll::new() {
            Ok(poll) => poll,
            Err(error) => {
                return Err(format!("Failed to init poll: {:?}", error));
            }
        };
        let mut subscribers = HashMap::new();
        let admin = admin::AdminPort::new(config.admin.clone(), &poll, &mut subscribers);

        let pools = HashMap::with_capacity(config.pools.len());

        let mut rustproxy = RustProxy {
            admin: admin,
            next_socket_index: next_socket_index,
            config: config,
            staged_config: None,
            backendpools: pools,
            backend_tokens: Rc::new(RefCell::new(HashMap::new())),
            backend_configs: HashMap::new(),
            subscribers: subscribers,
            written_sockets: Box::new(VecDeque::new()),
            poll: poll,
            timeout_queue : VecDeque::new(),
        };
        // Populate backend pools.
        let pools_config = rustproxy.config.pools.clone();
        for (pool_name, pool_config) in pools_config {
            rustproxy.init_backend_pool(&pool_name, &pool_config);
        }
        debug!("initialized rustproxy");

        Ok(rustproxy)
    }

    pub fn switch_config(&mut self) -> Result<(), String> {
        if self.staged_config.is_none() {
            return Err("No staged config".to_owned());
        }
        // Check that configs aren't the same.
        {
            match self.staged_config {
                Some(ref staged_config) => {
                    if staged_config == &self.config {
                        return Err("The configs are the same!".to_owned());
                    }
                }
                None => {}
            }
        }
        let staged_config = mem::replace(&mut self.staged_config, None);
        self.config = staged_config.unwrap();

        // Replace admin.
        if self.config.admin != self.admin.config {
            let admin = admin::AdminPort::new(self.config.admin.clone(), &self.poll, &mut self.subscribers);
            self.admin = admin; // TODO: what to do with old admin?
        }

        // Remove pools if they do not exist in config.
        let mut expired_pools = Vec::new();
        for pool_token in self.backendpools.keys() {
            let ref config = self.backendpools.get(&pool_token).unwrap().config;
            let mut should_keep = false;
            for (_, p_config) in self.config.pools.iter() {
                if p_config == config {
                    should_keep = true;
                    break;
                }
            }
            if !should_keep {
                expired_pools.push(pool_token.clone());
            }
        }
        for pool_token in expired_pools {
            self.remove_pool(pool_token.clone());
        }

        // Add pools if they are new.
        for (pool_name, pool_config) in self.config.clone().pools.iter() {
            if self.backend_configs.contains_key(&pool_config) {
                // Do we really need to set the name? Shouldn't they be set properly already?
                let pool = self.backendpools.get_mut(self.backend_configs.get(&pool_config).unwrap()).unwrap();
                pool.name = pool_name.clone();
            } else {
                // Create backend pool.
                self.init_backend_pool(pool_name, pool_config);
            }
        }

        // Clean up registries?
        Ok(())
    }

    pub fn run(&mut self) {
        let mut events = Events::with_capacity(1024);
        let mut timeout = None;
        loop {
            match self.poll.poll(&mut events, timeout) {
                Ok(_poll_size) => {}
                Err(error) => {
                    panic!("Error polling. Shutting down: {:?}", error);
                }
            };
            for event in events.iter() {
                debug!("Event detected: {:?} {:?}", &event.token(), event.readiness());
                self.handle_event(&event);
            }
            self.write_to_sockets();
            timeout = self.handle_timeouts();
        }
    }

    fn write_to_sockets(&mut self) {
        loop {
            let temp = self.written_sockets.pop_front();
            debug!("Flushed writing to sockets.");
            let (stream_token, stream_type) = match temp {
                Some(socket_token) => socket_token,
                None => break,
            };
            match stream_type {
                StreamType::AdminClient => {
                    match self.admin.client_sockets.get_mut(&stream_token) {
                        Some(stream) => {
                            let _ = stream.flush();
                        }
                        None => {
                            debug!("write_to_sockets: AdminClient {:?} no longer registered. Did a switch_config occur?", stream_token);
                        }
                    }
                }
                StreamType::PoolClient => {
                    match self.subscribers.get(&stream_token) {
                        Some(sub) => {
                            let subscriber = sub.clone();
                            match subscriber {
                                Subscriber::PoolClient(pool_token) => {
                                    let pool = self.backendpools.get_mut(&pool_token).unwrap();
                                    debug!("Writing out to {:?}", stream_token);
                                    let _ = pool.client_sockets.get_mut(&stream_token).unwrap().flush();
                                }

                                _ => panic!("write_to_sockets: Mismatch between StreamType and Subscriber: {:?}. Shutting down.", stream_token),
                            }
                        }
                        None => {
                            debug!("write_to_sockets: PoolClient {:?} no longer registered as a subscriber. Did a switch_config occur?", stream_token);
                        }
                    }
                }
                StreamType::PoolServer => {
                    match self.backend_tokens.borrow_mut().get_mut(&stream_token) {
                        Some(p_token) => {
                            let pool_token = p_token.clone();
                            let pool = self.backendpools.get_mut(&pool_token).unwrap();
                            let backend = pool.get_backend(stream_token);
                            backend.flush_stream();
                        }
                        None => {
                            debug!("write_to_sockets: PoolServer {:?} no longer registered. Did a switch_config occur?", stream_token);
                        }
                    }
                }
            }
        }
    }

    fn handle_event(&mut self, event: &Event) {
        if event.readiness().contains(UnixReady::error()) {
            // TODO: Don't want to do mark backend down for client connections.
            /* Why does the errror occur? How does a backend socket just error? Timeout? Is this on establishing connection?*/
            // TODO: We want to make sure these tokens that fail are actualy backend tokens. It could be something else, like timers.
            let token = event.token();
            let backend_tokens = self.backend_tokens.borrow();
            let pool_token = match backend_tokens.get(&token) {
                Some(pool_token) => pool_token,
                None => {
                    error!("Unable to find backend_token for error token: {:?}", token);
                    return;
                }
            };
            let pool = match self.backendpools.get_mut(&pool_token) {
                Some(pool) => pool,
                None => {
                    error!("Unable to find pool for pool token: {:?}", pool_token);
                    return;
                }
            };
            let backend = match pool.backend_map.get_mut(&token) {
                Some(backend) => backend,
                None => {
                    error!("Unable to find backend from token: {:?}", token);
                    return;
                }
            };
            backend.handle_backend_failure(token, &mut self.subscribers, &mut self.written_sockets, &mut self.poll);
            return;
        }
        match event.token() {
            SERVER => {
                // Need to increment the socket index.
                self.admin.accept_client_connection(&self.next_socket_index, &mut self.poll, &mut self.subscribers);
            }
            token => {
                let subscriber = match self.subscribers.get(&token) {
                    Some(subscriber) => subscriber.clone(),
                    None => {
                        debug!("Subscriber no contain key: {:?}", token);
                        return;
                    }
                };

                debug!("Got a subscriber token: {:?}", token);
                match subscriber {
                    Subscriber::Timeout(pool_token) => {
                        debug!("Timeout though?");
                        match self.backendpools.get_mut(&pool_token.clone()) {
                            Some(pool) => {
                                let backend_token = Token(token.0 - 1);
                                pool.handle_reconnect(&mut self.poll, &mut self.subscribers, backend_token)
                            }
                            None => error!("Hashmap says it has token but it really doesn't! {:?}",subscriber),
                        }
                        return;
                    }
                    Subscriber::PoolListener => {
                        match self.backendpools.get_mut(&token) {
                            Some(pool) => pool.accept_client_connection(&self.next_socket_index, &mut self.subscribers, &mut self.poll, token),
                            None => error!("Hashmap says it has token but it really doesn't!"),
                        }
                        return;
                    }
                    Subscriber::AdminClient => {
                        self.handle_client_socket(token);
                        return;
                    }
                    Subscriber::PoolClient(pool_token) => {
                        match self.backendpools.get_mut(&pool_token) {
                            Some(pool) => pool.handle_client(&mut self.written_sockets,
                                                             &mut self.timeout_queue,
                                                             token),
                            None => error!("Hashmap says it has token but it really doesn't!"),
                        }
                        return;
                    }
                    Subscriber::PoolServer(pool_token) => {
                        match self.backendpools.get_mut(&pool_token) {
                            Some(pool) => pool.get_backend(token).handle_backend_response(token, &mut self.poll, &mut self.subscribers),
                            None => error!("Hashmap says it has token but it really doesn't!"),
                        }
                        return;
                    }
                    _ => {
                        debug!("Subscriber was not associated with any subscriber type!");
                        return;
                    }
                }
            }
        }
    }

    fn handle_timeouts(&mut self) -> Option<Duration>{
        let timeout;
        let now = Instant::now();

        debug!("Timeout queue: {:?}", self.timeout_queue);
        // Get the next timeout.
        loop {
            // What we want right here is to 1. resolve any expired requests, and 2. calculate the next timeout we should wait for.
            // Right now we have each backend queue up its list of timeout.
            match self.timeout_queue.get(0) {
                Some(&(next_timeout, token)) => {
                    debug!("Next timeout's token: {:?} at {:?} Now: {:?}", token, next_timeout, now);
                    let pool_token = match self.backend_tokens.borrow().get(&token) {
                        Some(pool_token) => pool_token.clone(),
                        None => {
                            debug!("Unrecognized timeout token: {:?}. Did switch_config occur recently?", token);
                            continue;
                        }
                    };
                    let pool = match self.backendpools.get_mut(&pool_token) {
                        Some(pool) => pool,
                        None => {
                            error!("A backend token mapping: {:?} pointed to a nonexistent pool!", pool_token);
                            continue;
                        }
                    };
                    let pool_next_timeout = pool.next_timeout(token);
                    debug!("Pools' next timeout: {:?}", pool_next_timeout);
                    if pool_next_timeout == Some(next_timeout) {
                        if now >= next_timeout {
                            info!("Handling timeout. delta: {:?}, token: {:?}", now - next_timeout, token);
                            pool.handle_timeout(&mut self.subscribers, &mut self.written_sockets, token, next_timeout);
                            self.timeout_queue.pop_front();
                        } else {
                            debug!("Setting timeout: {:?}", next_timeout.duration_since(now));
                            timeout = Some(next_timeout.duration_since(now));
                            break;
                        }
                    } else {
                        if pool_next_timeout > Some(next_timeout) {
                            self.timeout_queue.pop_front();
                            // If timeout is less than the next thing, then it's a problem. It's a bad timeout and it should be ignored.
                            // However, if it's greater than, then it means that the next queued response doesn't have a timeout (for some reason).
                            // But shouldn't every request have a corresponding timeout? Perhaps not?
                        } else {
                            // If next queued response is before timeout, go for it.
                            debug!("The next timeout for the pool is {:?}", pool_next_timeout);
                            timeout = Some(pool_next_timeout.unwrap().duration_since(now));
                            break;
                        }
                    }
                }
                None => {
                    debug!("none left");
                    timeout = None;
                    break;
                }
            };
        }
        return timeout;
    }

    fn init_backend_pool(
        &mut self,
        pool_name: &String,
        pool_config: &BackendPoolConfig)
    {
        let pool_token = Token(self.get_socket_index());
        let pool = backendpool::BackendPool::new(pool_name.clone(), pool_token, pool_config.clone());
        self.backendpools.insert(pool_token, pool);

        let ref mut backendpools = self.backendpools;
        
        let moved_pool = match backendpools.get_mut(&pool_token) {
            Some(pool) => pool,
            None => {
                panic!("This should be impossible. The pool was just inserted into the map");
            }
        };
        moved_pool.connect(&self.backend_tokens, &self.next_socket_index, &mut self.poll, &mut self.subscribers, &mut self.written_sockets);

        self.backend_configs.insert(pool_config.clone(), pool_token);
    }

    fn remove_pool(&mut self, pool_token: Token) {
        self.backendpools.remove(&pool_token);

        self.backend_tokens.borrow_mut().retain(|&_, token| token != &pool_token);
        self.backend_configs.retain(|&_, token| token != &pool_token);
        
        self.subscribers.retain(
            |&token, subscriber| -> bool {
                match subscriber {
                    &mut Subscriber::Timeout(timeout_token) => {
                        return timeout_token != pool_token;
                    }
                    &mut Subscriber::PoolListener => {
                        return token != pool_token;
                    }
                    &mut Subscriber::PoolClient(p_token) => {
                        return p_token != pool_token;
                    }
                    &mut Subscriber::PoolServer(p_token) => {
                        return p_token != pool_token;
                    }
                    _ => {
                    }
                }
                true
            }
        );

        // written_sockets may refer to streams associated with removed pools. Those arre ignored, and a debug log emitted.
        // timeout_queue may also refer to expired pools. We should ignore then?
    }

    fn get_socket_index(&mut self) -> usize {
        self.next_socket_index.set(self.next_socket_index.get() + SOCKET_INDEX_SHIFT);
        info!("Getting a new token: {:?}", self.next_socket_index.get());
        self.next_socket_index.get()
    }

    pub fn load_config(&mut self, full_config_path: String) -> Result<(), String> {
        let config = load_config(full_config_path).unwrap();
        self.staged_config = Some(config);
        Ok(())
    }

    pub fn get_current_config(&self) -> RustProxyConfig {
        self.config.clone()
    }
    
    pub fn get_staged_config(&self) -> Option<RustProxyConfig> {
        self.staged_config.clone()
    }

    pub fn handle_client_socket(&mut self, token: ClientToken) {
        let mut switching_config = false;
        {
            let command = {
                let client_stream = match self.admin.client_sockets.get_mut(&token) {
                    Some(stream) => stream,
                    None => {
                        error!("AdminClient {:?} triggered an event, but it is no longer stored.", token);
                        return;
                    }
                };
                parse_redis_command(client_stream)
            };
            debug!("RECEIVED COMMAND: {}", command);
            let mut lines = command.lines();
            let current_line = lines.next();
            let res = match current_line {
                None => {
                    error!("AdminClient socket has nothing, when something was expected.");
                    return;
                }
                Some("INFO") => {
                    "DERP".to_owned()
                }
                Some("PING") => {
                    "PONG".to_owned()
                }
                Some("LOADCONFIG") => {
                    let next_line = lines.next();
                    if next_line.is_none() {
                        "Missing filepath argument!".to_owned()
                    } else {
                        let argument = next_line.unwrap();
                        self.load_config(argument.to_owned()).unwrap();
                        argument.to_owned()
                    }
                }
                Some("SHUTDOWN") => {
                    process::exit(0);
                }
                Some("STAGEDCONFIG") => {
                    let staged_config = self.get_staged_config();
                    if staged_config.is_none() {
                        "No config staged.".to_owned()
                    } else {
                        toml::to_string(&staged_config).unwrap()
                    }
                }
                Some("CONFIGINFO") => {
                    toml::to_string(&self.get_current_config()).unwrap()
                }
                Some("SWITCHCONFIG") => {
                    // TODO: Need to lose reference to the stream, OR
                    // best is to orphan it. and respond OK.
                    switching_config = true;
                    // need to respond to socket later.switch_config(rustproxy
                    "OK".to_owned()
                }
                Some(unknown_command) => {
                    debug!("Unknown command: {}", unknown_command);
                    "Unknown command".to_owned()
                }
            };
            if !switching_config {
                let mut response = String::new();
                response.push_str("+");
                response.push_str(res.as_str());
                response.push_str("\r\n");
                //response = "$-1\r\n".to_owned();
                debug!("RESPONSE: {}", &response);
                self.admin.write_to_client(token, response, &mut self.written_sockets);
                //let client_stream = rustproxy.admin.client_sockets.get_mut(&token).unwrap();
                //let _ = client_stream.write(&response.into_bytes()[..]);
                //rustproxy.written_sockets.push_back((token, StreamType::AdminClient));
            }   
        } 
        if switching_config {
            let result = {
                self.switch_config()
            };
            match result {
                Ok(_) => {
                    let mut response = String::new();
                    response.push_str("+");
                    response.push_str("OK");
                    response.push_str("\r\n");
                    self.admin.write_to_client(token, response, &mut self.written_sockets);

                }
                Err(message) => {
                    let mut response = String::new();
                    response.push_str("-");
                    response.push_str(message.as_str());
                    response.push_str("\r\n");
                    self.admin.write_to_client(token, response, &mut self.written_sockets);

                }
            }
        }
    }
}
