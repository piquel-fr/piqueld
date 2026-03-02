use piquelcore::ipc::ConnectionType;

pub struct Connection {
    connection_type: ConnectionType,
}

impl Connection {
    pub fn get_type(&self) -> ConnectionType {
        self.connection_type
    }
}
