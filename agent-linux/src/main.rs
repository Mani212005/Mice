//! Linux platform-agent scaffold. It deliberately implements only the shared
//! stdio handshake today; PipeWire, xdg-desktop-portal, and libei surfaces are
//! future platform work and must not be advertised before they exist.

use std::io::{BufReader, BufWriter, stdin, stdout};

use mice_ipc::{
    Capabilities, InitializeParams, PROTOCOL_VERSION, RpcNotification, RpcRequest, read_frame,
    write_frame,
};

fn initialize_request() -> RpcRequest {
    RpcRequest {
        jsonrpc: "2.0".into(),
        id: 1,
        method: "initialize".into(),
        params: serde_json::to_value(InitializeParams {
            protocol_version: PROTOCOL_VERSION.into(),
            platform: "linux".into(),
            capabilities: Capabilities {
                screen_capture: false,
                ax_read: false,
                inject_text: false,
                overlay: false,
                local_ocr: false,
                browser_bridge: false,
            },
        })
        .expect("initialize parameters are serializable"),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = BufWriter::new(stdout());
    write_frame(&mut writer, &initialize_request())?;

    let mut reader = BufReader::new(stdin());
    while let Ok(notification) = read_frame::<RpcNotification>(&mut reader) {
        if notification.method == "agent.stop" {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_stub_uses_the_shared_handshake_without_claiming_surfaces() {
        let request = initialize_request();
        let parameters: InitializeParams = serde_json::from_value(request.params).unwrap();

        assert_eq!(request.method, "initialize");
        assert_eq!(parameters.platform, "linux");
        assert!(!parameters.capabilities.screen_capture);
        assert!(!parameters.capabilities.ax_read);
        assert!(!parameters.capabilities.overlay);
    }
}
