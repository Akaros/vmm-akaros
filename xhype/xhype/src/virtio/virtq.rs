/* SPDX-License-Identifier: GPL-2.0-only */

use super::{
    channel, AddressConverter, IrqSender, Sender, TaskReceiver, TaskSender, VirtqReceiver,
    VirtqSender,
};
use std::mem::size_of;
use std::sync::{Arc, RwLock};

// https://docs.oasis-open.org/virtio/virtio/v1.1/csprd01/listings/virtio_queue.h

/* This marks a buffer as continuing via the next field. */
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/* This marks a buffer as write-only (otherwise read-only). */
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
/* This means the buffer contains a list of buffer descriptors. */
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/* The device uses this in used->flags to advise the driver: don't kick me
 * when you add a buffer.  It's unreliable, so it's simply an
 * optimization. */
pub const VIRTQ_USED_F_NO_NOTIFY: u16 = 1;
/* The driver uses this in avail->flags to advise the device: don't
 * interrupt me when you consume a buffer.  It's unreliable, so it's
 * simply an optimization.  */
pub const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;

/* Support for indirect descriptors */
pub const VIRTIO_F_INDIRECT_DESC: u16 = 28;

/* Support for avail_event and used_event fields */
pub const VIRTIO_F_EVENT_IDX: u16 = 29;

/* Arbitrary descriptor layouts. */
pub const VIRTIO_F_ANY_LAYOUT: u16 = 27;

pub const VIRTQ_SIZE_MAX: u16 = 1 << 15;

#[repr(C, packed)]
pub struct VirtqDesc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

#[repr(C, packed)]
pub struct VirtqUsedElem {
    pub id: u32,
    pub len: u32,
}

#[derive(Debug, Clone)]
pub struct Virtq<T> {
    pub num: u32,
    pub desc: T,
    pub avail: T,
    pub used: T,
}

impl<T> Virtq<T>
where
    T: Default,
{
    pub fn new(num: u32) -> Self {
        Virtq {
            num,
            desc: T::default(),
            avail: T::default(),
            used: T::default(),
        }
    }
}

// Virtq where all addresses are guest physical addresses
impl Virtq<u64> {
    pub fn to_hva<C>(&self, convert: C) -> Virtq<usize>
    where
        C: Fn(u64) -> usize,
    {
        Virtq {
            num: self.num,
            desc: convert(self.desc),
            avail: convert(self.avail),
            used: convert(self.used),
        }
    }
}

// Virtq where all addresses are host virtual addresses
impl Virtq<usize> {
    pub fn read_desc(&self, index: u16) -> VirtqDesc {
        let real_index = index as usize % self.num as usize;
        let ptr = (self.desc + size_of::<VirtqDesc>() * real_index) as *const VirtqDesc;
        unsafe { ptr.read() }
    }

    pub fn push_used(&self, id: u16, len: u32) {
        let used_index = self.used_index();
        let real_used_index = used_index as usize % self.num as usize;
        let used_elem = VirtqUsedElem { id: id as u32, len };
        let ptr_elem =
            (self.used + 4 + size_of::<VirtqUsedElem>() * real_used_index) as *mut VirtqUsedElem;
        unsafe {
            ptr_elem.write(used_elem);
        }
        self.set_used_index(used_index.wrapping_add(1));
    }

    pub fn used_flags(&self) -> u16 {
        let ptr = self.used as *const u16;
        unsafe { ptr.read() }
    }

    pub fn used_index(&self) -> u16 {
        let ptr = (self.used + 2) as *const u16;
        unsafe { ptr.read() }
    }

    fn set_used_index(&self, index: u16) {
        let ptr = (self.used + 2) as *mut u16;
        unsafe { ptr.write(index) }
    }

    pub fn set_used_flags(&self, flags: u16) {
        let ptr = self.used as *mut u16;
        unsafe { ptr.write(flags) }
    }

    pub fn read_avail(&self, index: u16) -> u16 {
        let real_index = index as usize % self.num as usize;
        let ptr = (self.avail + 4 + size_of::<u16>() * real_index) as *const u16;
        unsafe { ptr.read() }
    }

    pub fn avail_flags(&self) -> u16 {
        let ptr = self.avail as *const u16;
        unsafe { ptr.read() }
    }

    pub fn avail_index(&self) -> u16 {
        let ptr = (self.avail + 2) as *const u16;
        unsafe { ptr.read() }
    }

    pub fn get_avail<C>(
        &self,
        index: u16,
        converter: C,
    ) -> (Vec<(usize, usize)>, Vec<(usize, usize)>)
    where
        C: Fn(u64) -> usize,
    {
        let desc_head = self.read_avail(index);
        let mut desc_index = desc_head;
        let mut readable = Vec::new();
        let mut writable = Vec::new();
        loop {
            let desc = self.read_desc(desc_index);
            if desc.flags & VIRTQ_DESC_F_WRITE == 0 {
                readable.push((converter(desc.addr), desc.len as usize));
            } else {
                writable.push((converter(desc.addr), desc.len as usize));
            }
            if desc.flags & VIRTQ_DESC_F_NEXT > 0 {
                desc_index = desc.next;
            } else {
                break;
            }
        }
        (readable, writable)
    }
}

pub struct VirtqManager {
    pub name: String,
    pub qnum_max: u32,
    pub qready: u32,
    pub virtq: Virtq<u64>,
    pub task_sender: TaskSender,
    pub virtq_sender: VirtqSender,
}

pub type VirtioVqSrv = Box<
    dyn Fn(TaskReceiver, VirtqReceiver, u32, IrqSender, Arc<RwLock<u32>>, AddressConverter)
        + Send
        + Sync
        + 'static,
>;

impl VirtqManager {
    pub fn new(
        name: String,
        qnum_max: u32,
        srv: VirtioVqSrv,
        irq: u32,
        irq_sender: Sender<u32>,
        isr: Arc<RwLock<u32>>,
        gpa2hva: AddressConverter,
    ) -> Self {
        let (task_sender, task_receiver) = channel();
        let (virtq_sender, virtq_receiver) = channel();
        std::thread::Builder::new()
            .name(name.clone())
            .spawn(move || srv(task_receiver, virtq_receiver, irq, irq_sender, isr, gpa2hva))
            .expect(&format!("cannot create thread for virtq {}", &name));
        VirtqManager {
            name,
            qnum_max,
            qready: 0,
            virtq: Virtq::new(0),
            task_sender: task_sender,
            virtq_sender,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn virtq_struct_size_test() {
        assert_eq!(size_of::<VirtqUsedElem>(), 8);
        assert_eq!(size_of::<VirtqDesc>(), 16);
    }
}
