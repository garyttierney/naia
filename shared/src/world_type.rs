use naia_socket_shared::PacketReader;

use super::{
    diff_mask::DiffMask,
    protocol_type::{ProtocolInserter, ProtocolType},
    replica_ref::{ReplicaDynRefWrapper, ReplicaMutWrapper, ReplicaRefWrapper},
    replicate::{Replicate, ReplicateSafe},
};

/// Structures that implement the WorldMutType trait will be able to be loaded
/// into the Server at which point the Server will use this interface to keep
/// the WorldMutType in-sync with it's own Entities/Components
pub trait WorldRefType<P: ProtocolType, E> {
    // Entities
    /// check whether entity exists
    fn has_entity(&self, entity: &E) -> bool;
    /// get a list of all entities in the World
    fn entities(&self) -> Vec<E>;

    // Components
    /// check whether entity contains component
    fn has_component<R: ReplicateSafe<P>>(&self, entity: &E) -> bool;
    /// check whether entity contains component, dynamically
    fn has_component_of_kind(&self, entity: &E, component_kind: &P::Kind) -> bool;
    /// gets an entity's component
    fn get_component<'a, R: ReplicateSafe<P>>(
        &'a self,
        entity: &E,
    ) -> Option<ReplicaRefWrapper<'a, P, R>>;
    /// gets an entity's component, dynamically
    fn get_component_of_kind(
        &self,
        entity: &E,
        component_kind: &P::Kind,
    ) -> Option<ReplicaDynRefWrapper<'_, P>>;
}

/// Structures that implement the WorldMutType trait will be able to be loaded
/// into the Server at which point the Server will use this interface to keep
/// the WorldMutType in-sync with it's own Entities/Components
pub trait WorldMutType<P: ProtocolType, E>: WorldRefType<P, E> + ProtocolInserter<P, E> {
    // Entities
    /// spawn an entity
    fn spawn_entity(&mut self) -> E;
    /// despawn an entity
    fn despawn_entity(&mut self, entity: &E);

    // Components
    /// gets all of an Entity's Components as a list of Kinds
    fn get_component_kinds(&mut self, entity: &E) -> Vec<P::Kind>;
    /// gets an entity's component
    fn get_component_mut<'a, R: ReplicateSafe<P>>(
        &'a mut self,
        entity: &E,
    ) -> Option<ReplicaMutWrapper<'a, P, R>>;
    /// reads an incoming stream into a component
    fn component_read_partial(
        &mut self,
        entity: &E,
        component_kind: &P::Kind,
        diff_mask: &DiffMask,
        reader: &mut PacketReader,
        packet_index: u16,
    );
    /// mirrors the state of the same component owned by two different entities
    /// (setting 1st entity's component to 2nd entity's component's state)
    fn mirror_components(
        &mut self,
        mutable_entity: &E,
        immutable_entity: &E,
        component_kind: &P::Kind,
    );
    /// insert a component
    fn insert_component<R: ReplicateSafe<P>>(&mut self, entity: &E, component_ref: R);
    /// remove a component
    fn remove_component<R: Replicate<P>>(&mut self, entity: &E) -> Option<R>;
    /// remove a component by kind
    fn remove_component_of_kind(&mut self, entity: &E, component_kind: &P::Kind) -> Option<P>;
}
