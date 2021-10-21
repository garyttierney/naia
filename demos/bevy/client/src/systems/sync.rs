use bevy::{ecs::system::Query, transform::components::Transform};

use naia_bevy_client::Ref;

use naia_bevy_demo_shared::protocol::Position;

pub fn sync(mut query: Query<(&Ref<Position>, &mut Transform)>) {
    for (pos_ref, mut transform) in query.iter_mut() {
        let pos = pos_ref.borrow();
        transform.translation.x = f32::from(*(pos.x.get()));
        transform.translation.y = f32::from(*(pos.y.get())) * -1.0;
    }
}
