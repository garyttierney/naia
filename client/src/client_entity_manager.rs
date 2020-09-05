use byteorder::{BigEndian, ReadBytesExt};
use log::{info, warn};
use naia_shared::{
    EntityType, EventType, LocalEntityKey, Manifest, PacketReader, SequenceBuffer, StateMask,
};
use std::collections::{hash_map::Iter, HashMap, VecDeque};

use super::client_entity_message::ClientEntityMessage;
use crate::command_receiver::CommandReceiver;

const PAWN_HISTORY_SIZE: u16 = 64;

#[derive(Debug)]
pub struct ClientEntityManager<U: EntityType> {
    local_entity_store: HashMap<LocalEntityKey, U>,
    queued_incoming_messages: VecDeque<ClientEntityMessage>,
    pawn_store: HashMap<LocalEntityKey, U>,
    pawn_history: HashMap<LocalEntityKey, SequenceBuffer<U>>,
}

impl<U: EntityType> ClientEntityManager<U> {
    pub fn new() -> Self {
        ClientEntityManager {
            queued_incoming_messages: VecDeque::new(),
            local_entity_store: HashMap::new(),
            pawn_store: HashMap::new(),
            pawn_history: HashMap::new(),
        }
    }

    pub fn process_data<T: EventType>(
        &mut self,
        manifest: &Manifest<T, U>,
        command_receiver: &mut CommandReceiver<T>,
        packet_tick: u16,
        packet_index: u16,
        reader: &mut PacketReader,
    ) {
        let buffer = reader.get_buffer();
        let cursor = reader.get_cursor();

        let entity_message_count = cursor.read_u8().unwrap();
        //info!("reading {} entity messages", entity_message_count);
        for _x in 0..entity_message_count {
            let message_type: u8 = cursor.read_u8().unwrap().into();

            match message_type {
                0 => {
                    // Creation
                    let naia_id: u16 = cursor.read_u16::<BigEndian>().unwrap().into();
                    let local_key: u16 = cursor.read_u16::<BigEndian>().unwrap().into();
                    let payload_length: u8 = cursor.read_u8().unwrap().into();
                    let payload_start_position: usize = cursor.position() as usize;
                    let payload_end_position: usize =
                        payload_start_position + (payload_length as usize);

                    let entity_payload = buffer[payload_start_position..payload_end_position]
                        .to_vec()
                        .into_boxed_slice();

                    match manifest.create_entity(naia_id, &entity_payload) {
                        Some(new_entity) => {
                            if self.local_entity_store.contains_key(&local_key) {
                                warn!("duplicate local key inserted");
                            } else {
                                //info!("creation of entity w/ key of {}", local_key);
                                self.local_entity_store.insert(local_key, new_entity);
                                self.queued_incoming_messages
                                    .push_back(ClientEntityMessage::Create(local_key));
                            }
                        }
                        _ => {}
                    }

                    cursor.set_position(payload_end_position as u64);
                }
                1 => {
                    // Deletion
                    let local_key = cursor.read_u16::<BigEndian>().unwrap().into();
                    self.local_entity_store.remove(&local_key);
                    self.queued_incoming_messages
                        .push_back(ClientEntityMessage::Delete(local_key));
                }
                2 => {
                    // Update
                    let local_key = cursor.read_u16::<BigEndian>().unwrap().into();

                    if let Some(entity_ref) = self.local_entity_store.get_mut(&local_key) {
                        let state_mask: StateMask = StateMask::read(cursor);
                        let payload_length: u8 = cursor.read_u8().unwrap().into();
                        let payload_start_position: usize = cursor.position() as usize;
                        let payload_end_position: usize =
                            payload_start_position + (payload_length as usize);

                        let entity_payload = buffer[payload_start_position..payload_end_position]
                            .to_vec()
                            .into_boxed_slice();

                        entity_ref.read_partial(&state_mask, &entity_payload, packet_index);

                        // if entity is a pawn, check it against it's history
                        if let Some(pawn_history) = self.pawn_history.get_mut(&local_key) {
                            if let Some(historical_pawn) = pawn_history.get(packet_tick) {
                                if !entity_ref.equals(historical_pawn) {
                                    // prediction error encountered!
                                    info!("XXXXX prediction error encountered XXXXX ");
                                    command_receiver.replay_commands(packet_tick, local_key);
                                } else {
                                    pawn_history.remove_until(packet_tick);
                                }
                            }
                        }

                        self.queued_incoming_messages
                            .push_back(ClientEntityMessage::Update(local_key));

                        cursor.set_position(payload_end_position as u64);
                    }
                }
                3 => {
                    // Assign Pawn
                    let local_key: u16 = cursor.read_u16::<BigEndian>().unwrap().into();

                    if let Some(entity_ref) = self.local_entity_store.get_mut(&local_key) {
                        self.pawn_store.insert(
                            local_key,
                            entity_ref.inner_ref().as_ref().borrow().get_typed_copy(),
                        );

                        self.pawn_history
                            .insert(local_key, SequenceBuffer::with_capacity(PAWN_HISTORY_SIZE));

                        self.queued_incoming_messages
                            .push_back(ClientEntityMessage::AssignPawn(local_key));
                    }
                }
                4 => {
                    // Unassign Pawn
                    let local_key: u16 = cursor.read_u16::<BigEndian>().unwrap().into();
                    self.pawn_store.remove(&local_key);
                    self.pawn_history.remove(&local_key);
                    self.queued_incoming_messages
                        .push_back(ClientEntityMessage::UnassignPawn(local_key));
                }
                _ => {}
            }
        }
    }

    pub fn pop_incoming_message(&mut self) -> Option<ClientEntityMessage> {
        return self.queued_incoming_messages.pop_front();
    }

    pub fn entities_iter(&self) -> Iter<'_, LocalEntityKey, U> {
        return self.local_entity_store.iter();
    }

    pub fn get_local_entity(&self, key: LocalEntityKey) -> Option<&U> {
        return self.local_entity_store.get(&key);
    }

    pub fn pawns_iter(&self) -> Iter<'_, LocalEntityKey, U> {
        return self.pawn_store.iter();
    }

    pub fn get_pawn(&self, key: LocalEntityKey) -> Option<&U> {
        return self.pawn_store.get(&key);
    }

    pub fn pawn_reset(&mut self, key: LocalEntityKey) {
        if let Some(entity_ref) = self.local_entity_store.get_mut(&key) {
            self.pawn_store.remove(&key);
            self.pawn_store.insert(
                key,
                entity_ref.inner_ref().as_ref().borrow().get_typed_copy(),
            );
        }
    }

    pub fn pawn_clear_history(&mut self, key: LocalEntityKey) {
        if let Some(pawn_history) = self.pawn_history.get_mut(&key) {
            pawn_history.clear();
        }
    }

    pub fn save_pawn_snapshots(&mut self, host_tick: u16) {
        let mut pawn_keys = Vec::new();
        pawn_keys.extend(self.pawns_iter().map(|(key, _)| key));
        for key in &pawn_keys {
            self.save_replay_snapshot(host_tick, key);
        }
    }

    pub fn save_replay_snapshot(&mut self, history_tick: u16, pawn_key: &LocalEntityKey) {
        if let Some(pawn_ref) = self.pawn_store.get(pawn_key) {
            if let Some(pawn_history) = self.pawn_history.get_mut(pawn_key) {
                pawn_history.insert(
                    history_tick,
                    pawn_ref.inner_ref().as_ref().borrow().get_typed_copy(),
                );
            }
        }
    }

    pub fn fill_history(&self, buffer: &mut Vec<U>) {
        for (key, sequence) in self.pawn_history.iter() {
            let oldest = sequence.oldest();
            for seq in oldest..sequence.sequence_num() {
                if let Some(pawn_ref) = sequence.get(seq) {
                    buffer.push(pawn_ref.inner_ref().as_ref().borrow().get_typed_copy());
                }
            }
        }
    }
}
