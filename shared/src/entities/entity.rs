use std::{
    any::TypeId,
    cell::RefCell,
    fmt::{Debug, Formatter, Result},
    rc::Rc,
};

use super::{entity_mutator::EntityMutator, entity_type::EntityType, state_mask::StateMask};

pub trait Entity<T: EntityType> {
    fn get_state_mask_size(&self) -> u8;
    fn get_typed_copy(&self) -> T;
    fn get_type_id(&self) -> TypeId;
    fn write(&self, out_bytes: &mut Vec<u8>);
    fn write_partial(&self, state_mask: &StateMask, out_bytes: &mut Vec<u8>);
    fn read_partial(&mut self, state_mask: &StateMask, in_bytes: &[u8]);
    fn set_mutator(&mut self, mutator: &Rc<RefCell<dyn EntityMutator>>);
}

impl<T: EntityType> Debug for dyn Entity<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        f.write_str("Entity")
    }
}
