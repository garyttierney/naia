use std::{
    any::TypeId,
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    panic,
};

use byteorder::{BigEndian, WriteBytesExt};
use ring::{hmac, rand};
use slotmap::DenseSlotMap;

use naia_server_socket::{Packet, PacketReceiver, PacketSender, ServerSocket};
pub use naia_shared::{
    wrapping_diff, ComponentRecord, Connection, ConnectionConfig, EntityKey, HostTickManager,
    ImplRef, Instant, KeyGenerator, LocalComponentKey, ManagerType, Manifest, PacketReader,
    PacketType, PropertyMutate, ProtocolType, Ref, Replicate, SharedConfig, StandardHeader, Timer,
    Timestamp,
};

use super::{
    client_connection::ClientConnection,
    error::NaiaServerError,
    event::Event,
    keys::component_key::ComponentKey,
    mut_handler::MutHandler,
    property_mutator::PropertyMutator,
    room::{room_key::RoomKey, Room},
    server_config::ServerConfig,
    tick_manager::TickManager,
    user::{user_key::UserKey, User},
};

/// A server that uses either UDP or WebRTC communication to send/receive
/// messages to/from connected clients, and syncs registered entities to
/// clients to whom they are in-scope
pub struct Server<T: ProtocolType> {
    connection_config: ConnectionConfig,
    manifest: Manifest<T>,
    socket_sender: PacketSender,
    socket_receiver: Box<dyn PacketReceiver>,
    global_component_store: DenseSlotMap<ComponentKey, T>,
    mut_handler: Ref<MutHandler>,
    users: DenseSlotMap<UserKey, User>,
    rooms: DenseSlotMap<RoomKey, Room>,
    address_to_user_key_map: HashMap<SocketAddr, UserKey>,
    client_connections: HashMap<UserKey, ClientConnection<T>>,
    outstanding_connects: VecDeque<UserKey>,
    outstanding_disconnects: VecDeque<UserKey>,
    outstanding_auths: VecDeque<(UserKey, T)>,
    outstanding_errors: VecDeque<NaiaServerError>,
    heartbeat_timer: Timer,
    connection_hash_key: hmac::Key,
    tick_manager: TickManager,
    entity_scope_map: HashMap<(RoomKey, UserKey, EntityKey), bool>,
    entity_key_generator: KeyGenerator<EntityKey>,
    entity_component_map: HashMap<EntityKey, Ref<ComponentRecord<ComponentKey>>>,
    component_entity_map: HashMap<ComponentKey, EntityKey>,
}

impl<U: ProtocolType> Server<U> {
    /// Create a new Server
    pub fn new(
        manifest: Manifest<U>,
        server_config: Option<ServerConfig>,
        shared_config: SharedConfig,
    ) -> Self {
        let mut server_config = match server_config {
            Some(config) => config,
            None => ServerConfig::default(),
        };
        server_config.socket_config.shared.link_condition_config =
            shared_config.link_condition_config.clone();

        let connection_config = ConnectionConfig::new(
            server_config.disconnection_timeout_duration,
            server_config.heartbeat_interval,
            server_config.ping_interval,
            server_config.rtt_sample_size,
        );

        let (socket_sender, socket_receiver) = ServerSocket::listen(server_config.socket_config);

        let clients_map = HashMap::new();
        let heartbeat_timer = Timer::new(connection_config.heartbeat_interval);

        let connection_hash_key =
            hmac::Key::generate(hmac::HMAC_SHA256, &rand::SystemRandom::new()).unwrap();

        Server {
            manifest,
            global_component_store: DenseSlotMap::with_key(),
            entity_scope_map: HashMap::new(),
            mut_handler: MutHandler::new(),
            socket_sender,
            socket_receiver,
            connection_config,
            users: DenseSlotMap::with_key(),
            rooms: DenseSlotMap::with_key(),
            connection_hash_key,
            client_connections: clients_map,
            address_to_user_key_map: HashMap::new(),
            outstanding_auths: VecDeque::new(),
            outstanding_connects: VecDeque::new(),
            outstanding_disconnects: VecDeque::new(),
            outstanding_errors: VecDeque::new(),
            heartbeat_timer,
            tick_manager: TickManager::new(shared_config.tick_interval),
            entity_key_generator: KeyGenerator::new(),
            entity_component_map: HashMap::new(),
            component_entity_map: HashMap::new(),
        }
    }

    /// Must be called regularly, maintains connection to and receives messages
    /// from all Clients
    pub fn receive(&mut self) -> VecDeque<Result<Event<U>, NaiaServerError>> {
        let mut events = VecDeque::new();

        // Need to run this to maintain connection with all clients, and receive packets
        // until none left
        self.maintain_socket();

        // new authorizations
        while let Some((user_key, auth_message)) = self.outstanding_auths.pop_front() {
            events.push_back(Ok(Event::Authorization(user_key, auth_message)));
        }

        // new connections
        while let Some(user_key) = self.outstanding_connects.pop_front() {
            if let Some(user) = self.users.get(user_key) {
                self.address_to_user_key_map.insert(user.address, user_key);

                let mut new_connection = ClientConnection::new(
                    user.address,
                    Some(&self.mut_handler),
                    &self.connection_config,
                );
                //new_connection.process_incoming_header(&header);
                Server::<U>::send_connect_accept_message(
                    &mut new_connection,
                    &mut self.socket_sender,
                );
                self.client_connections.insert(user_key, new_connection);

                events.push_back(Ok(Event::Connection(user_key)));
            }
        }

        // new disconnections
        while let Some(user_key) = self.outstanding_disconnects.pop_front() {
            for (_, room) in self.rooms.iter_mut() {
                room.unsubscribe_user(&user_key);
            }

            let address = self.users.get(user_key).unwrap().address;
            self.address_to_user_key_map.remove(&address);
            let user_clone = self.users.get(user_key).unwrap().clone();
            self.users.remove(user_key);
            self.client_connections.remove(&user_key);

            events.push_back(Ok(Event::Disconnection(user_key, user_clone)));
        }

        // TODO: have 1 single queue for commands/messages from all users, as it's
        // possible this current technique unfairly favors the 1st users in
        // self.client_connections
        for (user_key, connection) in self.client_connections.iter_mut() {
            //receive commands from anyone
            while let Some((pawn_key, command)) =
                connection.get_incoming_command(self.tick_manager.get_tick())
            {
                events.push_back(Ok(Event::CommandEntity(*user_key, pawn_key, command)));
            }
            //receive messages from anyone
            while let Some(message) = connection.get_incoming_message() {
                events.push_back(Ok(Event::Message(*user_key, message)));
            }
        }

        // new errors
        while let Some(err) = self.outstanding_errors.pop_front() {
            events.push_back(Err(err));
        }

        // tick event
        if self.tick_manager.should_tick() {
            events.push_back(Ok(Event::Tick));
        }

        events
    }

    /// Accepts an incoming Client User, allowing them to establish a connection
    /// with the Server
    pub fn accept_connection(&mut self, user_key: &UserKey) {
        self.outstanding_connects.push_back(*user_key);
    }

    /// Rejects an incoming Client User, terminating their attempt to establish
    /// a connection with the Server
    pub fn reject_connection(&mut self, user_key: &UserKey) {
        self.users.remove(*user_key);
    }

    /// Queues up an Message to be sent to the Client associated with a given
    /// UserKey
    pub fn queue_message<T: ImplRef<U>>(
        &mut self,
        user_key: &UserKey,
        message_ref: &T,
        guaranteed_delivery: bool,
    ) {
        if let Some(connection) = self.client_connections.get_mut(user_key) {
            let dyn_ref = message_ref.dyn_ref();
            connection.queue_message(&dyn_ref, guaranteed_delivery);
        }
    }

    /// Sends all update messages to all Clients. If you don't call this
    /// method, the Server will never communicate with it's connected
    /// Clients
    pub fn send_all_updates(&mut self) {
        // update entity scopes
        self.update_entity_scopes();

        // loop through all connections, send packet
        for (user_key, connection) in self.client_connections.iter_mut() {
            if let Some(user) = self.users.get(*user_key) {
                connection.collect_component_updates();
                while let Some(payload) =
                    connection.get_outgoing_packet(self.tick_manager.get_tick(), &self.manifest)
                {
                    self.socket_sender
                        .send(Packet::new_raw(user.address, payload));
                    connection.mark_sent();
                }
            }
        }
    }

    /// Register an Entity with the Server initializing the Entity with a list
    /// Component Refs. The Server will sync the Entity to all connected
    /// Clients for which the Entity is in scope. Gives back an EntityKey
    /// which can be used to get the reference to the Entity from the Server
    /// once again
    pub fn register_entity_with_components<T: ImplRef<U>>(
        &mut self,
        component_refs: &[T],
    ) -> EntityKey {
        let entity_key = self.register_entity();
        for component_ref in component_refs {
            self.add_component_to_entity(&entity_key, component_ref);
        }
        return entity_key;
    }

    /// Register an Entity with the Server, whereby the Server will sync the
    /// given Entity's Components to all connected Clients for which the
    /// Entity is in scope. Gives back an EntityKey which can be used to get
    /// the reference to the Entity from the Server once again
    pub fn register_entity(&mut self) -> EntityKey {
        let entity_key: EntityKey = self.entity_key_generator.generate();
        self.entity_component_map
            .insert(entity_key, Ref::new(ComponentRecord::new()));
        return entity_key;
    }

    /// Deregisters an Entity with the Server, deleting local copies of the
    /// Entity on each Client
    pub fn deregister_entity(&mut self, key: &EntityKey) {
        for (user_key, _) in self.users.iter() {
            if let Some(user_connection) = self.client_connections.get_mut(&user_key) {
                Self::user_remove_entity(user_connection, key);
            }
        }

        self.entity_component_map.remove(key);
    }

    /// Assigns an Entity to a specific User, making it a Pawn for that User
    /// (meaning that the User will be able to issue Commands to that Pawn)
    pub fn assign_pawn_entity(&mut self, user_key: &UserKey, entity_key: &EntityKey) {
        // check that entity is initialized
        if let Some(entity_component_record) = self.entity_component_map.get(entity_key) {
            if let Some(user_connection) = self.client_connections.get_mut(user_key) {
                // add entity to user's connection
                Self::user_add_entity(
                    &self.global_component_store,
                    user_connection,
                    entity_key,
                    &entity_component_record,
                );

                // assign entity to user as a Pawn
                user_connection.add_pawn_entity(entity_key);
            }
        }
    }

    /// Unassigns a Pawn from a specific User (meaning that the User will be
    /// unable to issue Commands to that Pawn)
    pub fn unassign_pawn_entity(&mut self, user_key: &UserKey, entity_key: &EntityKey) {
        if self.entity_component_map.contains_key(entity_key) {
            if let Some(user_connection) = self.client_connections.get_mut(user_key) {
                user_connection.remove_pawn_entity(entity_key);
            }
        }
    }

    /// Register a Component with the Server, whereby the Server will sync the
    /// Component to all connected Clients for which the Component's Entity
    /// is in Scope. Gives back a ComponentKey which can be used to get the
    /// reference to the Component from the Server once again
    pub fn add_component_to_entity<T: ImplRef<U>>(
        &mut self,
        entity_key: &EntityKey,
        component_ref: &T,
    ) -> ComponentKey {
        if !self.entity_component_map.contains_key(&entity_key) {
            panic!("attempted to add component to non-existent entity");
        }

        let component_key: ComponentKey = self.register_component(component_ref);

        let dyn_ref: Ref<dyn Replicate<U>> = component_ref.dyn_ref();

        // add component to connections already tracking entity
        for (user_key, _) in self.users.iter() {
            if let Some(user_connection) = self.client_connections.get_mut(&user_key) {
                if user_connection.has_entity(entity_key) {
                    Self::user_add_component(user_connection, entity_key, &component_key, &dyn_ref);
                }
            }
        }

        self.component_entity_map.insert(component_key, *entity_key);

        if let Some(entity_component_record) = self.entity_component_map.get_mut(&entity_key) {
            entity_component_record
                .borrow_mut()
                .insert_component(&component_key, dyn_ref.borrow().get_type_id());
        }

        return component_key;
    }

    /// Deregisters a Component with the Server, deleting local copies of the
    /// Component on each Client
    pub fn remove_component(&mut self, component_key: &ComponentKey) -> U {
        for (user_key, _) in self.users.iter() {
            if let Some(user_connection) = self.client_connections.get_mut(&user_key) {
                Self::user_remove_component(user_connection, component_key);
            }
        }

        let entity_key = self
            .component_entity_map
            .remove(component_key)
            .expect("attempting to remove a component which does not exist");
        self.entity_component_map
            .get(&entity_key)
            .expect("component error during initialization causing issue with removal of component")
            .borrow_mut()
            .remove_component(component_key);

        self.mut_handler
            .borrow_mut()
            .deregister_component(component_key);
        return self
            .global_component_store
            .remove(*component_key)
            .expect("component not initialized correctly?");
    }

    /// Given an ComponentKey, get a reference to a registered Component being
    /// tracked by the Server
    pub fn get_component_by_key(&self, key: &ComponentKey) -> Option<&U> {
        return self.global_component_store.get(*key);
    }

    /// Given an EntityKey & a Component type, get a reference to a registered
    /// Component being tracked by the Server
    pub fn get_component_by_type<T: Replicate<U>>(&self, key: &EntityKey) -> Option<Ref<T>> {
        if let Some(component_record) = self.entity_component_map.get(key) {
            if let Some(component_key) = component_record
                .borrow()
                .get_key_from_type(&TypeId::of::<T>())
            {
                if let Some(component_protocol) = self.global_component_store.get(*component_key) {
                    return component_protocol.to_typed_ref::<T>();
                }
            }
        }
        return None;
    }

    /// Iterate through all the Server's Entities
    pub fn entities_iter(&self) -> Vec<EntityKey> {
        let mut output = Vec::<EntityKey>::new();
        // TODO: make this more efficient by some fancy 'collect' chaining type method?
        for entity_key in self.entity_component_map.keys() {
            output.push(*entity_key);
        }
        return output;
    }

    /// Iterate through all ComponentKeys associated a given Entity
    pub fn components_iter(&self, entity_key: &EntityKey) -> Vec<ComponentKey> {
        let mut output = Vec::<ComponentKey>::new();

        // TODO: make this more efficient by some fancy 'collect' chaining type method?
        if let Some(component_record) = self.entity_component_map.get(entity_key) {
            for component_key in component_record.borrow().get_component_keys() {
                output.push(component_key);
            }
        }

        return output;
    }

    /// Creates a new Room on the Server, returns a Key which can be used to
    /// reference said Room
    pub fn create_room(&mut self) -> RoomKey {
        let new_room = Room::new();
        return self.rooms.insert(new_room);
    }

    /// Deletes the Room associated with a given RoomKey on the Server
    pub fn delete_room(&mut self, key: &RoomKey) {
        self.rooms.remove(*key);
    }

    /// Gets a Room given an associated RoomKey
    pub fn get_room(&self, key: &RoomKey) -> Option<&Room> {
        return self.rooms.get(*key);
    }

    /// Gets a mutable Room given an associated RoomKey
    pub fn get_room_mut(&mut self, key: &RoomKey) -> Option<&mut Room> {
        return self.rooms.get_mut(*key);
    }

    /// Iterate through all the Server's current Rooms
    pub fn rooms_iter(&self) -> slotmap::dense::Iter<RoomKey, Room> {
        return self.rooms.iter();
    }

    /// Get the number of Rooms in the Server
    pub fn get_rooms_count(&self) -> usize {
        return self.rooms.len();
    }

    /// Add an User to a Room, given the appropriate RoomKey & UserKey
    /// Entities will only ever be in-scope for Users which are in a
    /// Room with them
    pub fn room_add_user(&mut self, room_key: &RoomKey, user_key: &UserKey) {
        if let Some(room) = self.rooms.get_mut(*room_key) {
            room.subscribe_user(user_key);
        }
    }

    /// Removes a User from a Room
    pub fn room_remove_user(&mut self, room_key: &RoomKey, user_key: &UserKey) {
        if let Some(room) = self.rooms.get_mut(*room_key) {
            room.unsubscribe_user(user_key);
        }
    }

    /// Add an Entity to a Room, given the appropriate RoomKey & EntityKey
    /// Entities will only ever be in-scope for Users which are in a Room with
    /// them
    pub fn room_add_entity(&mut self, room_key: &RoomKey, entity_key: &EntityKey) {
        if let Some(room) = self.rooms.get_mut(*room_key) {
            room.add_entity(entity_key);
        }
    }

    /// Remove an Entity from a Room, given the appropriate RoomKey & EntityKey
    pub fn room_remove_entity(&mut self, room_key: &RoomKey, entity_key: &EntityKey) {
        if let Some(room) = self.rooms.get_mut(*room_key) {
            room.remove_entity(entity_key);
        }
    }

    /// Used to evaluate whether, given a User & Entity that are in the
    /// same Room, said Entity should be in scope for the given User.
    ///
    /// While Rooms allow for a very simple scope to which an Entity can belong,
    /// this provides complete customization for advanced scopes.
    pub fn entity_set_scope(
        &mut self,
        room_key: &RoomKey,
        user_key: &UserKey,
        entity_key: &EntityKey,
        in_scope: bool,
    ) {
        let key = (*room_key, *user_key, *entity_key);
        self.entity_scope_map.insert(key, in_scope);
    }

    /// Return a collection of Entity Scope Sets, being a unique combination of
    /// a related Room, User, and Entity, used to determine which entities to
    /// replicate to which users
    pub fn entity_scope_sets(&self) -> Vec<(RoomKey, UserKey, EntityKey)> {
        let mut list: Vec<(RoomKey, UserKey, EntityKey)> = Vec::new();

        // TODO: precache this, instead of generating a new list every call
        // likely this is called A LOT
        for (room_key, room) in self.rooms.iter() {
            for user_key in room.users_iter() {
                for entity_key in room.entities_iter() {
                    list.push((room_key, *user_key, *entity_key));
                }
            }
        }

        return list;
    }

    /// Iterate through all currently connected Users
    pub fn users_iter(&self) -> slotmap::dense::Iter<UserKey, User> {
        return self.users.iter();
    }

    /// Get a User, given the associated UserKey
    pub fn get_user(&self, user_key: &UserKey) -> Option<&User> {
        return self.users.get(*user_key);
    }

    /// Get the number of Users currently connected
    pub fn get_users_count(&self) -> usize {
        return self.users.len();
    }

    /// Gets the last received tick from the Client
    pub fn get_client_tick(&self, user_key: &UserKey) -> Option<u16> {
        if let Some(user_connection) = self.client_connections.get(user_key) {
            return Some(user_connection.get_last_received_tick());
        }
        return None;
    }

    /// Gets the current tick of the Server
    pub fn get_server_tick(&self) -> u16 {
        self.tick_manager.get_tick()
    }

    /// Returns true if a given User has an Entity with a given EntityKey
    /// in-scope currently
    pub fn user_scope_has_entity(&self, user_key: &UserKey, entity_key: &EntityKey) -> bool {
        if let Some(user_connection) = self.client_connections.get(user_key) {
            return user_connection.has_entity(entity_key);
        }

        return false;
    }

    // Private methods

    fn maintain_socket(&mut self) {
        // heartbeats
        if self.heartbeat_timer.ringing() {
            self.heartbeat_timer.reset();

            for (user_key, connection) in self.client_connections.iter_mut() {
                if let Some(user) = self.users.get(*user_key) {
                    if connection.should_drop() {
                        self.outstanding_disconnects.push_back(*user_key);
                    } else {
                        if connection.should_send_heartbeat() {
                            // Don't try to refactor this to self.internal_send, doesn't seem to
                            // work cause of iter_mut()
                            let payload = connection.process_outgoing_header(
                                self.tick_manager.get_tick(),
                                connection.get_last_received_tick(),
                                PacketType::Heartbeat,
                                &[],
                            );
                            self.socket_sender
                                .send(Packet::new_raw(user.address, payload));
                            connection.mark_sent();
                        }
                    }
                }
            }
        }

        //receive socket events
        loop {
            match self.socket_receiver.receive() {
                Ok(Some(packet)) => {
                    let address = packet.address();
                    if let Some(user_key) = self.address_to_user_key_map.get(&address) {
                        match self.client_connections.get_mut(&user_key) {
                            Some(connection) => {
                                connection.mark_heard();
                            }
                            None => {} //not yet established connection
                        }
                    }

                    let (header, payload) = StandardHeader::read(packet.payload());

                    match header.packet_type() {
                        PacketType::ClientChallengeRequest => {
                            let mut reader = PacketReader::new(&payload);
                            let timestamp = Timestamp::read(&mut reader);

                            let mut timestamp_bytes = Vec::new();
                            timestamp.write(&mut timestamp_bytes);
                            let timestamp_hash: hmac::Tag =
                                hmac::sign(&self.connection_hash_key, &timestamp_bytes);

                            let mut payload_bytes = Vec::new();
                            // write current tick
                            payload_bytes
                                .write_u16::<BigEndian>(self.tick_manager.get_tick())
                                .unwrap();

                            //write timestamp
                            payload_bytes.append(&mut timestamp_bytes);

                            //write timestamp digest
                            let hash_bytes: &[u8] = timestamp_hash.as_ref();
                            for hash_byte in hash_bytes {
                                payload_bytes.push(*hash_byte);
                            }

                            Server::<U>::internal_send_connectionless(
                                &mut self.socket_sender,
                                PacketType::ServerChallengeResponse,
                                Packet::new(address, payload_bytes),
                            );
                        }
                        PacketType::ClientConnectRequest => {
                            let mut reader = PacketReader::new(&payload);
                            let timestamp = Timestamp::read(&mut reader);

                            if let Some(user_key) = self.address_to_user_key_map.get(&address) {
                                // At this point, we have already sent the ServerConnectResponse
                                // message, but we continue to send
                                // the message till the Client stops sending
                                // the ClientConnectRequest
                                if self.client_connections.contains_key(user_key) {
                                    let user = self.users.get(*user_key).unwrap();
                                    if user.timestamp == timestamp {
                                        let mut connection =
                                            self.client_connections.get_mut(user_key).unwrap();
                                        connection.process_incoming_header(&header);
                                        Server::<U>::send_connect_accept_message(
                                            &mut connection,
                                            &mut self.socket_sender,
                                        );
                                    } else {
                                        self.outstanding_disconnects.push_back(*user_key);
                                    }
                                } else {
                                    error!("if there's a user key associated with the address, should also have a client connection initiated");
                                }
                            } else {
                                //Verify that timestamp hash has been written by this
                                // server instance
                                let mut timestamp_bytes: Vec<u8> = Vec::new();
                                timestamp.write(&mut timestamp_bytes);
                                let mut digest_bytes: Vec<u8> = Vec::new();
                                for _ in 0..32 {
                                    digest_bytes.push(reader.read_u8());
                                }
                                if hmac::verify(
                                    &self.connection_hash_key,
                                    &timestamp_bytes,
                                    &digest_bytes,
                                )
                                .is_ok()
                                {
                                    let user = User::new(address, timestamp);
                                    let user_key = self.users.insert(user);

                                    // Return authorization event
                                    let naia_id = reader.read_u16();

                                    let auth_message =
                                        self.manifest.create_replica(naia_id, &mut reader);

                                    self.outstanding_auths.push_back((user_key, auth_message));
                                }
                            }
                        }
                        PacketType::Data => {
                            if let Some(user_key) = self.address_to_user_key_map.get(&address) {
                                match self.client_connections.get_mut(user_key) {
                                    Some(connection) => {
                                        connection.process_incoming_header(&header);
                                        connection.process_incoming_data(
                                            self.tick_manager.get_tick(),
                                            header.host_tick(),
                                            &self.manifest,
                                            &payload,
                                        );
                                    }
                                    None => {
                                        warn!(
                                            "received data from unauthenticated client: {}",
                                            address
                                        );
                                    }
                                }
                            }
                        }
                        PacketType::Heartbeat => {
                            if let Some(user_key) = self.address_to_user_key_map.get(&address) {
                                match self.client_connections.get_mut(user_key) {
                                    Some(connection) => {
                                        // Still need to do this so that proper notify
                                        // events fire based on the heartbeat header
                                        connection.process_incoming_header(&header);
                                    }
                                    None => {
                                        warn!(
                                            "received heartbeat from unauthenticated client: {}",
                                            address
                                        );
                                    }
                                }
                            }
                        }
                        PacketType::Ping => {
                            if let Some(user_key) = self.address_to_user_key_map.get(&address) {
                                match self.client_connections.get_mut(user_key) {
                                    Some(connection) => {
                                        connection.process_incoming_header(&header);
                                        let ping_payload = connection.process_ping(&payload);
                                        let payload_with_header = connection
                                            .process_outgoing_header(
                                                self.tick_manager.get_tick(),
                                                connection.get_last_received_tick(),
                                                PacketType::Pong,
                                                &ping_payload,
                                            );
                                        self.socket_sender.send(Packet::new_raw(
                                            connection.get_address(),
                                            payload_with_header,
                                        ));
                                        connection.mark_sent();
                                    }
                                    None => {
                                        warn!(
                                            "received ping from unauthenticated client: {}",
                                            address
                                        );
                                    }
                                }
                            }
                        }
                        PacketType::ServerChallengeResponse
                        | PacketType::ServerConnectResponse
                        | PacketType::Pong
                        | PacketType::Unknown => {
                            // do nothing
                        }
                    }
                }
                Ok(None) => {
                    // No more packets, break loop
                    break;
                }
                Err(error) => {
                    self.outstanding_errors
                        .push_back(NaiaServerError::Wrapped(Box::new(error)));
                }
            }
        }
    }

    fn update_entity_scopes(&mut self) {
        for (room_key, room) in self.rooms.iter_mut() {
            while let Some((removed_user, removed_entity)) = room.pop_entity_removal_queue() {
                if let Some(user_connection) = self.client_connections.get_mut(&removed_user) {
                    Self::user_remove_entity(user_connection, &removed_entity);
                }
            }

            for user_key in room.users_iter() {
                for entity_key in room.entities_iter() {
                    if self.entity_component_map.contains_key(entity_key) {
                        if let Some(user_connection) = self.client_connections.get_mut(user_key) {
                            let currently_in_scope = user_connection.has_entity(entity_key);

                            let should_be_in_scope: bool;
                            if user_connection.has_pawn_entity(entity_key) {
                                should_be_in_scope = true;
                            } else {
                                let key = (room_key, *user_key, *entity_key);
                                if let Some(in_scope) = self.entity_scope_map.get(&key) {
                                    should_be_in_scope = *in_scope;
                                } else {
                                    should_be_in_scope = false;
                                }
                            }

                            if should_be_in_scope {
                                if !currently_in_scope {
                                    // get a reference to the component map
                                    let entity_component_record =
                                        self.entity_component_map.get(entity_key).unwrap();

                                    // add entity to the connections local scope
                                    Self::user_add_entity(
                                        &self.global_component_store,
                                        user_connection,
                                        entity_key,
                                        &entity_component_record,
                                    );
                                }
                            } else {
                                if currently_in_scope {
                                    // remove entity from the connections local scope
                                    Self::user_remove_entity(user_connection, entity_key);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Register an Component with the Server, whereby the Server will the Component
    // to all connected Clients for which the Component is in scope. Gives back
    // an ComponentKey which can be used to get the reference to the Component
    // from the Server once again
    fn register_component<T: ImplRef<U>>(&mut self, component_ref: &T) -> ComponentKey {
        let dyn_ref = component_ref.dyn_ref();
        let new_mutator_ref: Ref<PropertyMutator> =
            Ref::new(PropertyMutator::new(&self.mut_handler));

        dyn_ref
            .borrow_mut()
            .set_mutator(&to_property_mutator(new_mutator_ref.clone()));

        let component_protocol = component_ref.protocol();
        let component_key = self.global_component_store.insert(component_protocol);
        new_mutator_ref
            .borrow_mut()
            .set_component_key(component_key);
        self.mut_handler
            .borrow_mut()
            .register_component(&component_key);
        return component_key;
    }

    fn user_remove_component(
        user_connection: &mut ClientConnection<U>,
        component_key: &ComponentKey,
    ) {
        //remove component from user connection
        user_connection.remove_component(component_key);
    }

    fn user_add_entity(
        component_store: &DenseSlotMap<ComponentKey, U>,
        user_connection: &mut ClientConnection<U>,
        entity_key: &EntityKey,
        entity_component_record: &Ref<ComponentRecord<ComponentKey>>,
    ) {
        // Get list of components first
        let mut component_list: Vec<(ComponentKey, Ref<dyn Replicate<U>>)> = Vec::new();
        for component_key in entity_component_record.borrow().get_component_keys() {
            if let Some(component_ref) = component_store.get(component_key) {
                component_list.push((component_key, component_ref.inner_ref().clone()));
            }
        }

        //add entity to user connection
        user_connection.add_entity(entity_key, entity_component_record, &component_list);
    }

    fn user_remove_entity(user_connection: &mut ClientConnection<U>, entity_key: &EntityKey) {
        //remove entity from user connection
        user_connection.remove_entity(entity_key);
    }

    fn user_add_component(
        user_connection: &mut ClientConnection<U>,
        entity_key: &EntityKey,
        component_key: &ComponentKey,
        component_ref: &Ref<dyn Replicate<U>>,
    ) {
        //add component to user connection
        user_connection.add_component(entity_key, component_key, component_ref);
    }

    fn send_connect_accept_message(
        connection: &mut ClientConnection<U>,
        sender: &mut PacketSender,
    ) {
        let payload =
            connection.process_outgoing_header(0, 0, PacketType::ServerConnectResponse, &[]);
        sender.send(Packet::new_raw(connection.get_address(), payload));
        connection.mark_sent();
    }

    fn internal_send_connectionless(
        sender: &mut PacketSender,
        packet_type: PacketType,
        packet: Packet,
    ) {
        let new_payload =
            naia_shared::utils::write_connectionless_payload(packet_type, packet.payload());
        sender.send(Packet::new_raw(packet.address(), new_payload));
    }
}

cfg_if! {
    if #[cfg(feature = "multithread")] {
        use std::sync::{Arc, Mutex};
        fn to_property_mutator_raw(eref: Arc<Mutex<PropertyMutator>>) -> Arc<Mutex<dyn PropertyMutate>> {
            eref.clone()
        }
    } else {
        use std::{cell::RefCell, rc::Rc};
        fn to_property_mutator_raw(eref: Rc<RefCell<PropertyMutator>>) -> Rc<RefCell<dyn PropertyMutate>> {
            eref.clone()
        }
    }
}

fn to_property_mutator(eref: Ref<PropertyMutator>) -> Ref<dyn PropertyMutate> {
    let upcast_ref = to_property_mutator_raw(eref.inner());
    Ref::new_raw(upcast_ref)
}
