#![no_std]
#![no_main]
#![feature(never_type)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use core::pin::Pin;
use core::ptr::NonNull;

use virtio_drivers::{
    device::blk::*,
    transport::{
        mmio::{MmioTransport, VirtIOHeader},
        DeviceType, Transport,
    },
};

use sel4_externally_shared::ExternallySharedRef;
use sel4_microkit::{memory_region_symbol, protection_domain, var, Channel, Handler};
use sel4_shared_ring_buffer::{RingBuffer, RingBuffers};
use sel4_shared_ring_buffer_block_io_types::{
    BlockIORequest, BlockIORequestStatus, BlockIORequestType,
};

use microkit_http_server_example_virtio_hal_impl::HalImpl;

const DEVICE: Channel = Channel::new(0);
const CLIENT: Channel = Channel::new(1);

// HACK hard-coded in virtio-drivers
const QUEUE_SIZE: usize = 4;

#[protection_domain(
    heap_size = 64 * 1024,
)]
fn init() -> HandlerImpl {
    HalImpl::init(
        *var!(virtio_blk_driver_dma_size: usize = 0),
        *var!(virtio_blk_driver_dma_vaddr: usize = 0),
        *var!(virtio_blk_driver_dma_paddr: usize = 0),
    );

    let mut dev = {
        let header = NonNull::new(
            (*var!(virtio_blk_mmio_vaddr: usize = 0) + *var!(virtio_blk_mmio_offset: usize = 0))
                as *mut VirtIOHeader,
        )
        .unwrap();
        let transport = unsafe { MmioTransport::new(header) }.unwrap();
        assert_eq!(transport.device_type(), DeviceType::Block);
        VirtIOBlk::<HalImpl, MmioTransport>::new(transport).unwrap()
    };

    let client_region = unsafe {
        ExternallySharedRef::<'static, _>::new(
            memory_region_symbol!(virtio_blk_client_dma_vaddr: *mut [u8], n = *var!(virtio_blk_client_dma_size: usize = 0)),
        )
    };

    let client_client_dma_region_paddr = *var!(virtio_blk_client_dma_paddr: usize = 0);

    let ring_buffers = unsafe {
        RingBuffers::<'_, fn() -> Result<(), !>, BlockIORequest>::new(
            RingBuffer::from_ptr(memory_region_symbol!(virtio_blk_free: *mut _)),
            RingBuffer::from_ptr(memory_region_symbol!(virtio_blk_used: *mut _)),
            notify_client,
            true,
        )
    };

    dev.ack_interrupt();
    DEVICE.irq_ack().unwrap();

    HandlerImpl {
        dev,
        client_region,
        client_client_dma_region_paddr,
        ring_buffers,
        pending: BTreeMap::new(),
    }
}

fn notify_client() -> Result<(), !> {
    CLIENT.notify();
    Ok::<_, !>(())
}

struct HandlerImpl {
    dev: VirtIOBlk<HalImpl, MmioTransport>,
    client_region: ExternallySharedRef<'static, [u8]>,
    client_client_dma_region_paddr: usize,
    ring_buffers: RingBuffers<'static, fn() -> Result<(), !>, BlockIORequest>,
    pending: BTreeMap<u16, Pin<Box<PendingEntry>>>,
}

struct PendingEntry {
    client_req: BlockIORequest,
    virtio_req: BlkReq,
    virtio_resp: BlkResp,
}

impl Handler for HandlerImpl {
    type Error = !;

    fn notified(&mut self, channel: Channel) -> Result<(), Self::Error> {
        match channel {
            DEVICE | CLIENT => {
                let mut notify = false;

                while self.dev.peek_used().is_some() {
                    let token = self.dev.peek_used().unwrap();
                    let mut pending_entry = self.pending.remove(&token).unwrap();
                    let buf_range = {
                        let start = pending_entry.client_req.buf().encoded_addr()
                            - self.client_client_dma_region_paddr;
                        let len = usize::try_from(pending_entry.client_req.buf().len()).unwrap();
                        start..start + len
                    };
                    let mut buf_ptr = self
                        .client_region
                        .as_mut_ptr()
                        .index(buf_range)
                        .as_raw_ptr();
                    unsafe {
                        let pending_entry = &mut *pending_entry;
                        self.dev
                            .complete_read_block(
                                token,
                                &pending_entry.virtio_req,
                                buf_ptr.as_mut(),
                                &mut pending_entry.virtio_resp,
                            )
                            .unwrap();
                    }
                    let status = match pending_entry.virtio_resp.status() {
                        RespStatus::OK => BlockIORequestStatus::Ok,
                        _ => panic!(),
                    };
                    let mut completed_req = pending_entry.client_req;
                    completed_req.set_status(status);
                    self.ring_buffers.used_mut().enqueue(completed_req).unwrap();
                    notify = true;
                }

                while self.pending.len() < QUEUE_SIZE && !self.ring_buffers.free().is_empty() {
                    let client_req = self.ring_buffers.free_mut().dequeue().unwrap();
                    assert_eq!(client_req.ty().unwrap(), BlockIORequestType::Read);
                    let mut pending_entry = Box::pin(PendingEntry {
                        client_req,
                        virtio_req: BlkReq::default(),
                        virtio_resp: BlkResp::default(),
                    });
                    let buf_range = {
                        let start =
                            client_req.buf().encoded_addr() - self.client_client_dma_region_paddr;
                        let len = usize::try_from(client_req.buf().len()).unwrap();
                        start..start + len
                    };
                    let mut buf_ptr = self
                        .client_region
                        .as_mut_ptr()
                        .index(buf_range)
                        .as_raw_ptr();
                    let token = unsafe {
                        let pending_entry = &mut *pending_entry;
                        self.dev
                            .read_block_nb(
                                pending_entry.client_req.block_id(),
                                &mut pending_entry.virtio_req,
                                buf_ptr.as_mut(),
                                &mut pending_entry.virtio_resp,
                            )
                            .unwrap()
                    };
                    assert!(self.pending.insert(token, pending_entry).is_none());
                    notify = true;
                }

                if notify {
                    self.ring_buffers.notify().unwrap();
                }

                self.dev.ack_interrupt();
                DEVICE.irq_ack().unwrap();
            }
            _ => {
                unreachable!()
            }
        }
        Ok(())
    }
}
