use std::{
    borrow::Borrow,
    clone::Clone,
    collections::{HashMap, HashSet, VecDeque},
    hash::Hash,
    net::SocketAddr,
    sync::{Arc, RwLock},
};

use byteorder::{BigEndian, WriteBytesExt};

use naia_shared::{
    DiffMask, KeyGenerator, LocalComponentKey, LocalEntity, NaiaKey, PacketNotifiable,
    ProtocolKindType, ProtocolType, WorldRefType, MTU_SIZE,
};

use super::{
    entity_action::EntityAction, global_diff_handler::GlobalDiffHandler, keys::ComponentKey,
    local_component_record::LocalComponentRecord, local_entity_record::LocalEntityRecord,
    locality_status::LocalityStatus, packet_writer::PacketWriter,
    user_diff_handler::UserDiffHandler, world_record::WorldRecord,
};

/// Manages Entities for a given Client connection and keeps them in
/// sync on the Client
pub struct EntityManager<P: ProtocolType, E: Copy + Eq + Hash> {
    address: SocketAddr,
    // Entities
    entity_generator: KeyGenerator<LocalEntity>,
    entity_records: HashMap<E, LocalEntityRecord>,
    local_to_global_entity_map: HashMap<LocalEntity, E>,
    delayed_entity_deletions: HashSet<E>,
    // Components
    diff_handler: UserDiffHandler,
    component_key_generator: KeyGenerator<LocalComponentKey>,
    local_to_global_component_key_map: HashMap<LocalComponentKey, ComponentKey>,
    component_records: HashMap<ComponentKey, LocalComponentRecord>,
    delayed_component_deletions: HashSet<ComponentKey>,
    // Actions / updates / ect
    queued_actions: VecDeque<EntityAction<P, E>>,
    sent_actions: HashMap<u16, Vec<EntityAction<P, E>>>,
    sent_updates: HashMap<u16, HashMap<ComponentKey, DiffMask>>,
    last_update_packet_index: u16,
    last_last_update_packet_index: u16,
    last_popped_diff_mask: Option<DiffMask>,
    last_popped_diff_mask_list: Option<Vec<(ComponentKey, DiffMask)>>,
    delivered_packets: VecDeque<u16>,
}

impl<P: ProtocolType, E: Copy + Eq + Hash> EntityManager<P, E> {
    /// Create a new EntityManager, given the client's address
    pub fn new(address: SocketAddr, diff_handler: &Arc<RwLock<GlobalDiffHandler>>) -> Self {
        EntityManager {
            address,
            // Entities
            entity_generator: KeyGenerator::new(),
            entity_records: HashMap::new(),
            local_to_global_entity_map: HashMap::new(),
            delayed_entity_deletions: HashSet::new(),
            // Components
            diff_handler: UserDiffHandler::new(diff_handler),
            component_key_generator: KeyGenerator::new(),
            local_to_global_component_key_map: HashMap::new(),
            component_records: HashMap::new(),
            delayed_component_deletions: HashSet::new(),
            // Actions / updates / ect
            queued_actions: VecDeque::new(),
            sent_actions: HashMap::new(),
            sent_updates: HashMap::<u16, HashMap<ComponentKey, DiffMask>>::new(),
            last_update_packet_index: 0,
            last_last_update_packet_index: 0,
            last_popped_diff_mask: None,
            last_popped_diff_mask_list: None,
            delivered_packets: VecDeque::new(),
        }
    }

    pub fn has_outgoing_actions(&self) -> bool {
        return self.queued_actions.len() != 0;
    }

    pub fn pop_outgoing_action<W: WorldRefType<P, E>>(
        &mut self,
        world_record: &WorldRecord<E, P::Kind>,
        packet_index: u16,
    ) -> Option<EntityAction<P, E>> {
        let queued_action_opt = self.queued_actions.pop_front();
        if queued_action_opt.is_none() {
            return None;
        }
        let action = {
            let queued_action = queued_action_opt.unwrap();
            if let EntityAction::SpawnEntity(global_entity, _) = queued_action {
                // get the most recent list of components in here ...
                if !world_record.has_entity(&global_entity) {
                    panic!("entity does not exist!")
                }

                let mut component_list = Vec::new();
                let mut diff_mask_list: Vec<(ComponentKey, DiffMask)> = Vec::new();

                let global_component_keys = world_record.get_component_keys(&global_entity);

                for global_component_key in global_component_keys {
                    let (_, component_kind) = world_record
                        .get_component_record(&global_component_key)
                        .expect("component not tracked by server?");

                    component_list.push((global_component_key, component_kind));

                    let diff_mask = self
                        .diff_handler
                        .get_diff_mask(&global_component_key)
                        .expect("DiffHandler does not have registered Component..")
                        .clone();

                    diff_mask_list.push((global_component_key, diff_mask));

                    self.diff_handler.clear_diff_mask(&global_component_key);
                }

                self.last_popped_diff_mask_list = Some(diff_mask_list);

                EntityAction::SpawnEntity(global_entity, component_list)
            } else {
                queued_action
            }
        };

        if !self.sent_actions.contains_key(&packet_index) {
            self.sent_actions.insert(packet_index, Vec::new());
        }

        if let Some(sent_actions_list) = self.sent_actions.get_mut(&packet_index) {
            sent_actions_list.push(action.clone());
        }

        //clear diff mask of component if need be
        match action {
            EntityAction::InsertComponent(_, global_component_key, _) => {
                self.pop_insert_component_diff_mask(&global_component_key);
            }
            EntityAction::UpdateComponent(
                global_entity,
                global_component_key,
                diff_mask,
                component_kind,
            ) => {
                return Some(self.pop_update_component_diff_mask(
                    packet_index,
                    global_entity,
                    &global_component_key,
                    &diff_mask,
                    component_kind,
                ));
            }
            _ => {}
        }

        return Some(action);
    }

    pub fn unpop_outgoing_action(&mut self, packet_index: u16, action: EntityAction<P, E>) {
        info!("unpopping");
        if let Some(sent_actions_list) = self.sent_actions.get_mut(&packet_index) {
            sent_actions_list.pop();
            if sent_actions_list.len() == 0 {
                self.sent_actions.remove(&packet_index);
            }
        }

        match action {
            EntityAction::SpawnEntity(_, _) => {
                if let Some(last_popped_diff_mask_list) = &self.last_popped_diff_mask_list {
                    for (global_component_key, last_popped_diff_mask) in last_popped_diff_mask_list
                    {
                        self.diff_handler
                            .set_diff_mask(global_component_key, &last_popped_diff_mask);
                    }
                }
                self.queued_actions.push_front(action);
                return;
            }
            EntityAction::InsertComponent(_, global_component_key, _) => {
                self.unpop_insert_component_diff_mask(&global_component_key);
                self.queued_actions.push_front(action);
                return;
            }
            EntityAction::UpdateComponent(
                global_entity,
                global_component_key,
                _,
                component_kind,
            ) => {
                let cloned_action = self.unpop_update_component_diff_mask(
                    packet_index,
                    global_entity,
                    &global_component_key,
                    component_kind,
                );
                self.queued_actions.push_front(cloned_action);
                return;
            }
            _ => {}
        }
    }

    // Entities

    pub fn spawn_entity(&mut self, world_record: &WorldRecord<E, P::Kind>, global_entity: &E) {
        if !self.entity_records.contains_key(global_entity) {
            // first, get a list of components
            // then, add components
            if !world_record.has_entity(global_entity) {
                panic!("entity nonexistant!");
            }
            for global_component_key in world_record.get_component_keys(global_entity) {
                self.component_init(&global_component_key, LocalityStatus::Creating);
            }

            // then, add entity
            let local_entity: LocalEntity = self.entity_generator.generate();
            self.local_to_global_entity_map
                .insert(local_entity, *global_entity);
            let local_entity_record = LocalEntityRecord::new(local_entity);
            self.entity_records
                .insert(*global_entity, local_entity_record);
            self.queued_actions
                .push_back(EntityAction::SpawnEntity(*global_entity, Vec::new()));
        } else {
            panic!("added entity twice");
        }
    }

    pub fn despawn_entity(&mut self, world_record: &WorldRecord<E, P::Kind>, global_entity: &E) {
        if self.has_entity_prediction(global_entity) {
            self.remove_prediction_entity(global_entity);
        }

        if let Some(entity_record) = self.entity_records.get_mut(global_entity) {
            match entity_record.status {
                LocalityStatus::Creating => {
                    // queue deletion action to be sent after creation
                    self.delayed_entity_deletions.insert(*global_entity);
                }
                LocalityStatus::Created => {
                    // send deletion action
                    entity_delete::<P, E>(&mut self.queued_actions, entity_record, global_entity);

                    // Entity deletion IS Component deletion, so update those component records
                    // accordingly
                    for global_component_key in world_record.get_component_keys(global_entity) {
                        if let Some(component_record) =
                            self.component_records.get_mut(&global_component_key)
                        {
                            component_record.status = LocalityStatus::Deleting;
                        }
                    }
                }
                LocalityStatus::Deleting => {
                    // deletion in progress, do nothing
                }
            }
        }
    }

    pub fn has_entity(&self, entity: &E) -> bool {
        return self.entity_records.contains_key(entity);
    }

    // Prediction Entities

    pub fn add_prediction_entity(&mut self, entity: &E) {
        let entity_record = self
            .entity_records
            .get_mut(entity)
            .expect("attempting to assign a nonexistent Entity");
        if entity_record.is_prediction {
            panic!("attempting to assign an Entity twice!");
        }

        // success
        entity_record.is_prediction = true;
        self.queued_actions
            .push_back(EntityAction::OwnEntity(*entity));
    }

    pub fn remove_prediction_entity(&mut self, entity: &E) {
        let entity_record = self
            .entity_records
            .get_mut(entity)
            .expect("attempting to disown on Entity which is not in-scope");
        if !entity_record.is_prediction {
            panic!("attempting to disown an Entity which is not currently assigned");
        }

        // success
        entity_record.is_prediction = false;
        self.queued_actions
            .push_back(EntityAction::DisownEntity(*entity));
    }

    pub fn has_entity_prediction(&self, entity: &E) -> bool {
        if let Some(entity_record) = self.entity_records.get(entity) {
            return entity_record.is_prediction;
        }
        return false;
    }

    // Components

    pub fn insert_component(
        &mut self,
        world_record: &WorldRecord<E, P::Kind>,
        component_key: &ComponentKey,
    ) {
        let (entity, component_kind) = world_record
            .get_component_record(component_key)
            .expect("component does not exist!");

        if !self.entity_records.contains_key(&entity) {
            panic!(
                "attempting to add Component to Entity that does not yet exist for this connection"
            );
        }

        self.component_init(component_key, LocalityStatus::Creating);

        let entity_record = self.entity_records.get(&entity).unwrap(); // checked this above

        match entity_record.status {
            LocalityStatus::Creating => {
                // uncreated Components will be created after Entity is
                // created
            }
            LocalityStatus::Created => {
                // send InsertComponent action
                self.queued_actions.push_back(EntityAction::InsertComponent(
                    entity,
                    *component_key,
                    component_kind,
                ));
            }
            LocalityStatus::Deleting => {
                // deletion in progress, do nothing
            }
        }
    }

    pub fn remove_component(&mut self, component_key: &ComponentKey) {
        let component_record = self.component_records.get_mut(component_key).expect(
            "attempting to remove a component from a connection within which it does not exist",
        );

        match component_record.status {
            LocalityStatus::Creating => {
                // queue deletion action to be sent after creation
                self.delayed_component_deletions.insert(*component_key);
            }
            LocalityStatus::Created => {
                // send deletion action
                component_delete::<P, E>(&mut self.queued_actions, component_record, component_key);
            }
            LocalityStatus::Deleting => {
                // deletion in progress, do nothing
            }
        }
    }

    // Ect..

    pub fn get_global_entity_from_local(&self, local_entity: LocalEntity) -> Option<&E> {
        return self.local_to_global_entity_map.get(&local_entity);
    }

    pub fn collect_component_updates(&mut self, world_record: &WorldRecord<E, P::Kind>) {
        for (component_key, record) in self.component_records.iter() {
            if record.status == LocalityStatus::Created
                && self.diff_handler.has_diff_mask(component_key)
            {
                let (entity, component_kind) = world_record
                    .get_component_record(component_key)
                    .expect("component does not exist!");

                let new_diff_mask = self
                    .diff_handler
                    .get_diff_mask(component_key)
                    .expect("DiffHandler does not have registered Component!")
                    .clone();
                self.queued_actions.push_back(EntityAction::UpdateComponent(
                    entity,
                    *component_key,
                    new_diff_mask,
                    component_kind,
                ));
            }
        }
    }

    pub fn write_entity_action<W: WorldRefType<P, E>>(
        &self,
        world: &W,
        packet_writer: &mut PacketWriter,
        action: &EntityAction<P, E>,
    ) -> bool {
        let mut action_total_bytes = Vec::<u8>::new();

        //Write EntityAction type
        action_total_bytes
            .write_u8(action.as_type().to_u8())
            .unwrap();

        match action {
            EntityAction::SpawnEntity(global_entity, component_list) => {
                let local_entity = self.entity_records.get(global_entity).unwrap().local_key;

                action_total_bytes
                    .write_u16::<BigEndian>(local_entity.to_u16())
                    .unwrap(); //write local entity

                // get list of components
                let components_num = component_list.len();
                if components_num > 255 {
                    panic!("no entity should have so many components... fix this");
                }
                action_total_bytes.write_u8(components_num as u8).unwrap(); //write number of components

                for (global_component_key, component_kind) in component_list {
                    let local_component_key = self
                        .component_records
                        .get(global_component_key)
                        .unwrap()
                        .local_key;

                    //write component payload
                    let component_ref = world
                        .get_component_of_kind(global_entity, component_kind)
                        .expect("Component does not exist in World");
                    let mut component_payload_bytes = Vec::<u8>::new();
                    component_ref.write(&mut component_payload_bytes);

                    //Write component "header"
                    action_total_bytes
                        .write_u16::<BigEndian>(component_kind.to_u16())
                        .unwrap(); // write naia id
                    action_total_bytes
                        .write_u16::<BigEndian>(local_component_key.to_u16())
                        .unwrap(); //write local component key
                    action_total_bytes.append(&mut component_payload_bytes);
                    // write payload
                }
            }
            EntityAction::DespawnEntity(global_entity) => {
                let local_entity = self.entity_records.get(global_entity).unwrap().local_key;
                action_total_bytes
                    .write_u16::<BigEndian>(local_entity.to_u16())
                    .unwrap(); //write local entity
            }
            EntityAction::OwnEntity(global_entity) => {
                let local_entity = self.entity_records.get(global_entity).unwrap().local_key;
                action_total_bytes
                    .write_u16::<BigEndian>(local_entity.to_u16())
                    .unwrap(); //write local entity
            }
            EntityAction::DisownEntity(global_entity) => {
                let local_entity = self.entity_records.get(global_entity).unwrap().local_key;
                action_total_bytes
                    .write_u16::<BigEndian>(local_entity.to_u16())
                    .unwrap(); //write local entity
            }
            EntityAction::InsertComponent(global_entity, global_component_key, component_kind) => {
                let local_entity = self.entity_records.get(global_entity).unwrap().local_key;
                let local_component_key = self
                    .component_records
                    .get(global_component_key)
                    .unwrap()
                    .local_key;

                //write component payload
                let component_ref = world
                    .get_component_of_kind(global_entity, component_kind)
                    .expect("Component does not exist in World");

                let mut component_payload_bytes = Vec::<u8>::new();
                component_ref.write(&mut component_payload_bytes);

                //Write component "header"
                action_total_bytes
                    .write_u16::<BigEndian>(local_entity.to_u16())
                    .unwrap(); //write local entity
                action_total_bytes
                    .write_u16::<BigEndian>(component_kind.to_u16())
                    .unwrap(); // write component kind
                action_total_bytes
                    .write_u16::<BigEndian>(local_component_key.to_u16())
                    .unwrap(); //write local component key
                action_total_bytes.append(&mut component_payload_bytes); // write payload
            }
            EntityAction::UpdateComponent(
                global_entity,
                global_component_key,
                diff_mask,
                component_kind,
            ) => {
                let local_component_key = self
                    .component_records
                    .get(global_component_key)
                    .unwrap()
                    .local_key;

                //write component payload
                let component_ref = world
                    .get_component_of_kind(global_entity, component_kind)
                    .expect("Component does not exist in World");

                let mut component_payload_bytes = Vec::<u8>::new();
                component_ref.write_partial(diff_mask, &mut component_payload_bytes);

                //Write component "header"
                action_total_bytes
                    .write_u16::<BigEndian>(local_component_key.to_u16())
                    .unwrap(); //write local component key
                diff_mask.write(&mut action_total_bytes); // write diff mask
                action_total_bytes.append(&mut component_payload_bytes); // write
                                                                         // payload
            }
            EntityAction::RemoveComponent(global_component_key) => {
                let local_component_key = self
                    .component_records
                    .get(global_component_key)
                    .unwrap()
                    .local_key;

                action_total_bytes
                    .write_u16::<BigEndian>(local_component_key.to_u16())
                    .unwrap(); //write local key
            }
        }

        let mut hypothetical_next_payload_size =
            packet_writer.bytes_number() + action_total_bytes.len();
        if packet_writer.entity_action_count == 0 {
            hypothetical_next_payload_size += 2;
        }
        if hypothetical_next_payload_size < MTU_SIZE {
            if packet_writer.entity_action_count == 255 {
                return false;
            }
            packet_writer.entity_action_count = packet_writer.entity_action_count.wrapping_add(1);
            packet_writer
                .entity_working_bytes
                .append(&mut action_total_bytes);
            return true;
        } else {
            return false;
        }
    }

    // Private methods

    fn component_init(
        &mut self,
        component_key: &ComponentKey,
        status: LocalityStatus,
    ) -> LocalComponentKey {
        if self.component_records.contains_key(component_key) {
            // Should panic, as this is not dependent on any unreliable transport factor
            panic!("attempted to add component twice..");
        }

        // create DiffMask
        self.diff_handler
            .register_component(&self.address, &component_key);

        // register Component with various indexes
        let local_component_key: LocalComponentKey = self.component_key_generator.generate();
        self.local_to_global_component_key_map
            .insert(local_component_key, *component_key);
        let component_record = LocalComponentRecord::new(local_component_key, status);
        self.component_records
            .insert(*component_key, component_record);
        return local_component_key;
    }

    fn component_cleanup(&mut self, global_component_key: &ComponentKey) {
        if let Some(component_record) = self.component_records.remove(global_component_key) {
            // actually delete the component from local records
            self.diff_handler.deregister_component(global_component_key);

            let local_component_key = component_record.local_key;
            self.local_to_global_component_key_map
                .remove(&local_component_key);
            self.component_key_generator
                .recycle_key(&local_component_key);
        } else {
            // likely due to duplicate delivered deletion actions
            warn!(
                "attempting to clean up component from connection inside which it is not present"
            );
        }
    }

    fn pop_insert_component_diff_mask(&mut self, global_component_key: &ComponentKey) {
        let new_diff_mask = self
            .diff_handler
            .get_diff_mask(global_component_key)
            .expect("DiffHandler doesn't have Component registered!")
            .clone();
        self.last_popped_diff_mask = Some(new_diff_mask);
        self.diff_handler.clear_diff_mask(global_component_key);
    }

    fn unpop_insert_component_diff_mask(&mut self, global_component_key: &ComponentKey) {
        if let Some(last_popped_diff_mask) = &self.last_popped_diff_mask {
            self.diff_handler
                .set_diff_mask(global_component_key, &last_popped_diff_mask);
        }
    }

    fn pop_update_component_diff_mask(
        &mut self,
        packet_index: u16,
        global_entity: E,
        global_component_key: &ComponentKey,
        diff_mask: &DiffMask,
        component_kind: P::Kind,
    ) -> EntityAction<P, E> {
        let locked_diff_mask =
            self.process_component_update(packet_index, global_component_key, diff_mask);
        // return new Update action to be written
        return EntityAction::UpdateComponent(
            global_entity,
            *global_component_key,
            locked_diff_mask,
            component_kind,
        );
    }

    fn unpop_update_component_diff_mask(
        &mut self,
        packet_index: u16,
        global_entity: E,
        global_component_key: &ComponentKey,
        component_kind: P::Kind,
    ) -> EntityAction<P, E> {
        let original_diff_mask = self.undo_component_update(&packet_index, &global_component_key);

        return EntityAction::UpdateComponent(
            global_entity,
            *global_component_key,
            original_diff_mask,
            component_kind,
        );
    }

    fn process_component_update(
        &mut self,
        packet_index: u16,
        global_component_key: &ComponentKey,
        diff_mask: &DiffMask,
    ) -> DiffMask {
        // previously the diff mask was the CURRENT diff mask for the
        // component, we want to lock that in so we know exactly what we're
        // writing
        let locked_diff_mask = diff_mask.clone();

        // place diff mask in a special transmission record - like map
        if !self.sent_updates.contains_key(&packet_index) {
            let sent_updates_map: HashMap<ComponentKey, DiffMask> = HashMap::new();
            self.sent_updates.insert(packet_index, sent_updates_map);
            self.last_last_update_packet_index = self.last_update_packet_index;
            self.last_update_packet_index = packet_index;
        }

        if let Some(sent_updates_map) = self.sent_updates.get_mut(&packet_index) {
            sent_updates_map.insert(*global_component_key, locked_diff_mask.clone());
        }

        // having copied the diff mask for this update, clear the component
        self.last_popped_diff_mask = Some(diff_mask.borrow().clone());
        self.diff_handler.clear_diff_mask(global_component_key);

        locked_diff_mask
    }

    fn undo_component_update(
        &mut self,
        packet_index: &u16,
        global_component_key: &ComponentKey,
    ) -> DiffMask {
        if let Some(sent_updates_map) = self.sent_updates.get_mut(packet_index) {
            sent_updates_map.remove(global_component_key);
            if sent_updates_map.len() == 0 {
                self.sent_updates.remove(&packet_index);
            }
        }

        self.last_update_packet_index = self.last_last_update_packet_index;
        if let Some(last_popped_diff_mask) = &self.last_popped_diff_mask {
            self.diff_handler
                .set_diff_mask(global_component_key, &last_popped_diff_mask);
        }

        self.diff_handler
            .get_diff_mask(global_component_key)
            .expect("uh oh, we don't have enough info to unpop the action")
            .clone()
    }

    pub fn process_delivered_packets(&mut self, world_record: &WorldRecord<E, P::Kind>) {
        while let Some(packet_index) = self.delivered_packets.pop_front() {
            let mut deleted_components: Vec<ComponentKey> = Vec::new();

            if let Some(delivered_actions_list) = self.sent_actions.remove(&packet_index) {
                for delivered_action in delivered_actions_list.into_iter() {
                    match delivered_action {
                        EntityAction::RemoveComponent(global_component_key) => {
                            deleted_components.push(global_component_key);
                        }
                        EntityAction::UpdateComponent(_, _, _, _) => {
                            self.sent_updates.remove(&packet_index);
                        }
                        EntityAction::SpawnEntity(global_entity, mut component_list) => {
                            let entity_record = self.entity_records.get_mut(&global_entity)
                                .expect("created entity does not have a entity_record ... initialization error?");

                            // do we need to delete this now?
                            if self.delayed_entity_deletions.remove(&global_entity) {
                                entity_delete::<P, E>(
                                    &mut self.queued_actions,
                                    entity_record,
                                    &global_entity,
                                );
                            } else {
                                // set to status of created
                                entity_record.status = LocalityStatus::Created;

                                // set status of components to created
                                while let Some((global_component_key, _)) = component_list.pop() {
                                    let component_record = self
                                        .component_records
                                        .get_mut(&global_component_key)
                                        .expect("component not created correctly?");
                                    component_record.status = LocalityStatus::Created;
                                }

                                // for any components on this entity that have not yet been created
                                // initiate that now
                                for global_component_key in
                                    world_record.get_component_keys(&global_entity)
                                {
                                    let component_record = self
                                        .component_records
                                        .get(&global_component_key)
                                        .expect("component not created correctly?");
                                    // check if component has been successfully created
                                    // (perhaps through the previous entity_create operation)
                                    if component_record.status == LocalityStatus::Creating {
                                        let (_, component_kind) = world_record
                                            .get_component_record(&global_component_key)
                                            .expect("component does not exist!");

                                        self.queued_actions.push_back(
                                            EntityAction::InsertComponent(
                                                global_entity,
                                                global_component_key,
                                                component_kind,
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                        EntityAction::DespawnEntity(global_entity) => {
                            let local_entity =
                                self.entity_records.get(&global_entity).unwrap().local_key;

                            // actually delete the entity from local records
                            self.entity_records.remove(&global_entity);
                            self.local_to_global_entity_map.remove(&local_entity);
                            self.entity_generator.recycle_key(&local_entity);

                            // delete all components associated with entity
                            for global_component_key in
                                world_record.get_component_keys(&global_entity)
                            {
                                deleted_components.push(global_component_key);
                            }
                        }
                        EntityAction::OwnEntity(_) => {}
                        EntityAction::DisownEntity(_) => {}
                        EntityAction::InsertComponent(_, global_component_key, _) => {
                            let component_record = self
                                .component_records
                                .get_mut(&global_component_key)
                                .expect(
                                    "added component does not have a record .. initiation problem?",
                                );
                            // do we need to delete this now?
                            if self
                                .delayed_component_deletions
                                .remove(&global_component_key)
                            {
                                component_delete::<P, E>(
                                    &mut self.queued_actions,
                                    component_record,
                                    &global_component_key,
                                );
                            } else {
                                // we do not need to delete just yet
                                component_record.status = LocalityStatus::Created;
                            }
                        }
                    }
                }
            }

            for deleted_component_key in deleted_components {
                self.component_cleanup(&deleted_component_key);
            }
        }
    }
}

impl<P: ProtocolType, E: Copy + Eq + Hash> PacketNotifiable for EntityManager<P, E> {
    fn notify_packet_delivered(&mut self, packet_index: u16) {
        self.delivered_packets.push_back(packet_index);
    }

    fn notify_packet_dropped(&mut self, dropped_packet_index: u16) {
        if let Some(dropped_actions_list) = self.sent_actions.get_mut(&dropped_packet_index) {
            for dropped_action in dropped_actions_list.drain(..) {
                match dropped_action {
                    // guaranteed delivery actions
                    EntityAction::SpawnEntity(_, _)
                    | EntityAction::DespawnEntity(_)
                    | EntityAction::OwnEntity(_)
                    | EntityAction::DisownEntity(_)
                    | EntityAction::InsertComponent(_, _, _)
                    | EntityAction::RemoveComponent(_) => {
                        self.queued_actions.push_back(dropped_action);
                    }
                    // non-guaranteed delivery actions
                    EntityAction::UpdateComponent(_, global_component_key, _, _) => {
                        if let Some(diff_mask_map) = self.sent_updates.get(&dropped_packet_index) {
                            if let Some(diff_mask) = diff_mask_map.get(&global_component_key) {
                                let mut new_diff_mask = diff_mask.borrow().clone();

                                // walk from dropped packet up to most recently sent packet
                                if dropped_packet_index != self.last_update_packet_index {
                                    let mut packet_index = dropped_packet_index.wrapping_add(1);
                                    while packet_index != self.last_update_packet_index {
                                        if let Some(diff_mask_map) =
                                            self.sent_updates.get(&packet_index)
                                        {
                                            if let Some(diff_mask) =
                                                diff_mask_map.get(&global_component_key)
                                            {
                                                new_diff_mask.nand(diff_mask.borrow().borrow());
                                            }
                                        }

                                        packet_index = packet_index.wrapping_add(1);
                                    }
                                }

                                self.diff_handler
                                    .or_diff_mask(&global_component_key, &new_diff_mask);
                            }
                        }
                    }
                }
            }

            self.sent_updates.remove(&dropped_packet_index);
            self.sent_actions.remove(&dropped_packet_index);
        }
    }
}

fn component_delete<P: ProtocolType, E: Copy>(
    queued_actions: &mut VecDeque<EntityAction<P, E>>,
    record: &mut LocalComponentRecord,
    component_key: &ComponentKey,
) {
    record.status = LocalityStatus::Deleting;

    queued_actions.push_back(EntityAction::RemoveComponent(*component_key));
}

fn entity_delete<P: ProtocolType, E: Copy>(
    queued_actions: &mut VecDeque<EntityAction<P, E>>,
    entity_record: &mut LocalEntityRecord,
    entity: &E,
) {
    entity_record.status = LocalityStatus::Deleting;

    queued_actions.push_back(EntityAction::DespawnEntity(*entity));
}
