use std::ops::Deref;

use bevy::ecs::entity::Entity as BevyEntity;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Entity(BevyEntity);

impl Entity {
    pub fn new(entity: BevyEntity) -> Self {
        return Entity(entity);
    }
}

impl Deref for Entity {
    type Target = BevyEntity;

    fn deref(&self) -> &Self::Target {
        return &self.0;
    }
}
