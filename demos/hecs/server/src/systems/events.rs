use naia_hecs_server::Event;

use crate::app::App;

pub fn process_events(app: &mut App) {
    for event in app.server.receive() {
        match event {
            Ok(Event::Authorization(user_key, Protocol::Auth(auth_message))) => {
                let username = auth_message.username.get();
                let password = auth_message.password.get();
                if username == "charlie" && password == "12345" {
                    // Accept incoming connection
                    app.server.accept_connection(&user_key);
                } else {
                    // Reject incoming connection
                    app.server.reject_connection(&user_key);
                }
            }
            Ok(Event::Connection(user_key)) => {
                let address = app
                    .server
                    .user_mut(&user_key)
                    .enter_room(&app.main_room_key)
                    .address();
                info!("Naia Server connected to: {}", address);
            }
            Ok(Event::Disconnection(_, user)) => {
                info!("Naia Server disconnected from: {:?}", user.address);
            }
            Ok(Event::Tick) => app.tick(),
            Err(error) => {
                info!("Naia Server Error: {}", error);
            }
            _ => {}
        }
    }
}
