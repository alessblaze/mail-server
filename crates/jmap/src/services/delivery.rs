/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::sync::Arc;

use common::{core::BuildServer, ipc::DeliveryEvent, Inner};
use tokio::sync::mpsc;

use super::ingest::MailDelivery;

pub fn spawn_delivery_manager(inner: Arc<Inner>, mut delivery_rx: mpsc::Receiver<DeliveryEvent>) {
    tokio::spawn(async move {
        while let Some(event) = delivery_rx.recv().await {
            dbg!(&event);
            match event {
                DeliveryEvent::Ingest { message, result_tx } => {
                    let session_id = message.session_id;
                    dbg!(session_id);
                    let server = inner.build_server();
                    dbg!(session_id);
                    let result = server.deliver_message(message).await;
                    dbg!(session_id);

                    result_tx.send(result).ok();
                    dbg!(session_id);
                }
                DeliveryEvent::Stop => break,
            }
        }
    });
}
