//! Cola de bloques compartida entre conexiones. Sin E/S: solo decide qué rango
//! descarga cada conexión, así que se prueba con pruebas unitarias simples.
//!
//! Reglas:
//! 1. Una conexión libre coge el bloque pendiente con menor desplazamiento.
//! 2. Si no quedan pendientes, parte por la mitad el bloque activo al que más
//!    le falta, siempre que cada mitad tenga al menos el tamaño mínimo.
//! 3. Si ya no se puede partir, compite con el bloque al que más le falta
//!    (preferiblemente uno que descarga otra red). Gana la primera conexión
//!    que llegue al final; las demás reciben la orden de parar.
//!
//! Competir es seguro porque todas las conexiones escriben exactamente los
//! mismos bytes en las mismas posiciones del archivo.

use serde::{Deserialize, Serialize};

use crate::NetworkId;

/// Máximo de conexiones trabajando a la vez en un mismo bloque.
const MAX_LEASES_PER_BLOCK: usize = 2;

/// Un trozo del archivo: `[start, end)`, con `written` bytes ya escritos de
/// forma contigua desde `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub start: u64,
    pub end: u64,
    pub written: u64,
}

impl Block {
    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn remaining(&self) -> u64 {
        self.len() - self.written
    }

    pub fn is_done(&self) -> bool {
        self.written >= self.len()
    }
}

/// Identificador de una asignación (una conexión trabajando en un bloque).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LeaseId(u64);

/// Trabajo asignado a una conexión: escribir `[start, end)` del bloque. La
/// petición HTTP pide `Range: bytes=start-(request_end-1)`, que puede abarcar
/// los bloques siguientes (ver [`BlockQueue::extend`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease {
    pub id: LeaseId,
    pub block: usize,
    pub start: u64,
    pub end: u64,
    pub request_end: u64,
}

/// Qué debe hacer una conexión tras confirmar bytes escritos.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Commit {
    /// Seguir; el bloque termina en `end` (puede haber encogido).
    Continue { end: u64 },
    /// Parar: el bloque está terminado (por esta conexión o por otra).
    Stop,
}

#[derive(Debug)]
struct ActiveLease {
    id: LeaseId,
    network: NetworkId,
    /// Siguiente posición absoluta que escribirá esta conexión.
    pos: u64,
}

#[derive(Debug)]
struct Slot {
    block: Block,
    leases: Vec<ActiveLease>,
}

impl Slot {
    /// Posición más avanzada escrita de forma contigua desde el inicio.
    fn frontier(&self) -> u64 {
        self.block.start + self.block.written
    }

    fn is_pending(&self) -> bool {
        !self.block.is_done() && self.leases.is_empty()
    }

    /// Lo que falta tras la conexión más adelantada.
    fn outstanding(&self) -> u64 {
        let furthest = self
            .leases
            .iter()
            .map(|l| l.pos)
            .max()
            .unwrap_or(self.frontier());
        self.block.end.saturating_sub(furthest.max(self.frontier()))
    }
}

/// Cola de bloques de un archivo.
#[derive(Debug)]
pub struct BlockQueue {
    slots: Vec<Slot>,
    min_block: u64,
    next_lease: u64,
}

impl BlockQueue {
    /// Reparte `size` bytes en bloques de `block_size`.
    pub fn new(size: u64, block_size: u64, min_block: u64) -> Self {
        let block_size = block_size.max(1);
        let mut blocks = Vec::new();
        let mut start = 0;
        while start < size {
            let end = (start + block_size).min(size);
            blocks.push(Block {
                start,
                end,
                written: 0,
            });
            start = end;
        }
        Self::from_blocks(blocks, min_block)
    }

    /// Reconstruye la cola desde un estado guardado.
    pub fn from_blocks(blocks: Vec<Block>, min_block: u64) -> Self {
        Self {
            slots: blocks
                .into_iter()
                .map(|block| Slot {
                    block,
                    leases: Vec::new(),
                })
                .collect(),
            min_block: min_block.max(1),
            next_lease: 0,
        }
    }

    /// Comprueba que los bloques cubren `[0, size)` sin huecos ni solapes.
    pub fn validate(blocks: &[Block], size: u64) -> bool {
        let mut sorted: Vec<_> = blocks.to_vec();
        sorted.sort_by_key(|b| b.start);
        let mut expected = 0;
        for b in &sorted {
            if b.start != expected || b.end < b.start || b.written > b.len() {
                return false;
            }
            expected = b.end;
        }
        expected == size
    }

    /// Asigna trabajo a una conexión de la red `network`, o `None` si todo lo
    /// que falta ya tiene suficientes conexiones.
    pub fn next(&mut self, network: NetworkId) -> Option<Lease> {
        // 1. Bloque pendiente sin nadie trabajando, dentro del tramo pendiente
        //    más grande. Si a ese tramo ya llega por detrás una conexión
        //    (que seguirá de largo con su misma petición), se empieza por la
        //    mitad; así las conexiones quedan repartidas y cada una avanza
        //    muchos bloques seguidos con una sola petición HTTP.
        if let Some((run, len)) = self.largest_pending_run() {
            let order = self.sorted_indices();
            let preceded_by_active = run > 0 && !self.slots[order[run - 1]].leases.is_empty();
            let pick = if preceded_by_active {
                run + len / 2
            } else {
                run
            };
            let idx = order[pick];
            let request_end = self.slots[order[run + len - 1]].block.end;
            let start = self.slots[idx].frontier();
            let mut lease = self.lease(idx, network, start);
            lease.request_end = request_end;
            return Some(lease);
        }

        // 2. Partir el bloque activo al que más le falta.
        let splittable = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.leases.len() == 1 && s.outstanding() >= 2 * self.min_block)
            .max_by_key(|(_, s)| s.outstanding())
            .map(|(i, _)| i);
        if let Some(idx) = splittable {
            let slot = &mut self.slots[idx];
            let from = slot.leases[0].pos.max(slot.frontier());
            let mid = from + (slot.block.end - from) / 2;
            let old_end = slot.block.end;
            slot.block.end = mid;
            // Lo ya escrito no puede pasar del nuevo final.
            slot.block.written = slot.block.written.min(slot.block.len());
            self.slots.push(Slot {
                block: Block {
                    start: mid,
                    end: old_end,
                    written: 0,
                },
                leases: Vec::new(),
            });
            let new_idx = self.slots.len() - 1;
            return Some(self.lease(new_idx, network, mid));
        }

        // 3. Competir con el bloque al que más le falta, prefiriendo los que
        //    no está descargando esta misma red.
        let race = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                !s.block.is_done()
                    && !s.leases.is_empty()
                    && s.leases.len() < MAX_LEASES_PER_BLOCK
                    && s.outstanding() > 0
            })
            .max_by_key(|(_, s)| {
                let other_network = s.leases.iter().all(|l| l.network != network);
                (other_network, s.outstanding())
            })
            .map(|(i, _)| i);
        if let Some(idx) = race {
            let slot = &self.slots[idx];
            let start = slot
                .leases
                .iter()
                .map(|l| l.pos)
                .max()
                .unwrap_or(slot.frontier())
                .max(slot.frontier());
            return Some(self.lease(idx, network, start));
        }
        None
    }

    fn lease(&mut self, idx: usize, network: NetworkId, start: u64) -> Lease {
        let id = LeaseId(self.next_lease);
        self.next_lease += 1;
        let slot = &mut self.slots[idx];
        slot.leases.push(ActiveLease {
            id,
            network,
            pos: start,
        });
        Lease {
            id,
            block: idx,
            start,
            end: slot.block.end,
            request_end: slot.block.end,
        }
    }

    /// Índices de los bloques ordenados por posición.
    fn sorted_indices(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.slots.len()).collect();
        order.sort_by_key(|&i| self.slots[i].block.start);
        order
    }

    /// Tramo de bloques pendientes seguidos con más bytes por bajar:
    /// (posición en el orden, número de bloques).
    fn largest_pending_run(&self) -> Option<(usize, usize)> {
        let order = self.sorted_indices();
        let mut best: Option<(usize, usize, u64)> = None;
        let mut i = 0;
        while i < order.len() {
            if !self.slots[order[i]].is_pending() {
                i += 1;
                continue;
            }
            let run = i;
            let mut bytes = 0;
            while i < order.len() && self.slots[order[i]].is_pending() {
                bytes += self.slots[order[i]].block.remaining();
                i += 1;
            }
            if best.is_none_or(|(_, _, b)| bytes > b) {
                best = Some((run, i - run, bytes));
            }
        }
        best.map(|(run, len, _)| (run, len))
    }

    /// Cuando una conexión termina su bloque y su petición HTTP sigue
    /// trayendo datos (hasta `request_end`), coge el bloque siguiente si nadie
    /// lo tiene y continúa sin abrir otra petición.
    pub fn extend(&mut self, prev: &Lease, network: NetworkId) -> Option<Lease> {
        let prev_end = self.slots.get(prev.block)?.block.end;
        if prev_end >= prev.request_end {
            return None;
        }
        let idx = self
            .slots
            .iter()
            .position(|s| s.block.start == prev_end && s.is_pending())?;
        self.release(prev);
        let mut lease = self.lease(idx, network, prev_end);
        lease.request_end = prev.request_end;
        Some(lease)
    }

    /// Final actual del bloque de una asignación, o `None` si debe parar.
    /// Llamar justo antes de escribir para recortar lo que sobre.
    pub fn allowed_end(&self, lease: &Lease) -> Option<u64> {
        let slot = self.slots.get(lease.block)?;
        if slot.block.is_done() || !slot.leases.iter().any(|l| l.id == lease.id) {
            return None;
        }
        Some(slot.block.end)
    }

    /// Confirma que la conexión ha escrito hasta `pos` (exclusivo). Devuelve
    /// cuántos bytes nuevos cuenta el bloque (para el progreso) y qué hacer.
    pub fn commit(&mut self, lease: &Lease, pos: u64) -> (u64, Commit) {
        let Some(slot) = self.slots.get_mut(lease.block) else {
            return (0, Commit::Stop);
        };
        let end = slot.block.end;
        let Some(active) = slot.leases.iter_mut().find(|l| l.id == lease.id) else {
            return (0, Commit::Stop);
        };
        active.pos = active.pos.max(pos.min(end));
        let before = slot.block.written;
        // Las conexiones que compiten empezaron donde iba la más adelantada,
        // así que la unión de lo escrito es contigua hasta la mayor posición.
        let furthest = active.pos;
        if furthest > slot.frontier() {
            slot.block.written = furthest - slot.block.start;
        }
        let gained = slot.block.written - before;
        if slot.block.is_done() {
            slot.leases.clear();
            (gained, Commit::Stop)
        } else {
            (gained, Commit::Continue { end })
        }
    }

    /// La conexión deja el bloque (error, cancelación o fin).
    pub fn release(&mut self, lease: &Lease) {
        if let Some(slot) = self.slots.get_mut(lease.block) {
            slot.leases.retain(|l| l.id != lease.id);
        }
    }

    /// Bytes escritos en total.
    pub fn written(&self) -> u64 {
        self.slots.iter().map(|s| s.block.written).sum()
    }

    /// ¿Está todo descargado?
    pub fn is_complete(&self) -> bool {
        self.slots.iter().all(|s| s.block.is_done())
    }

    /// Copia de los bloques para guardarla en disco.
    pub fn snapshot(&self) -> Vec<Block> {
        let mut blocks: Vec<_> = self.slots.iter().map(|s| s.block).collect();
        blocks.sort_by_key(|b| b.start);
        blocks
    }

    /// Número de bloques (crece al partir bloques).
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N0: NetworkId = NetworkId(0);
    const N1: NetworkId = NetworkId(1);
    const MIB: u64 = 1024 * 1024;

    #[test]
    fn splits_into_blocks() {
        let q = BlockQueue::new(20 * MIB + 5, 8 * MIB, MIB);
        let b = q.snapshot();
        assert_eq!(b.len(), 3);
        assert_eq!(b[2].end, 20 * MIB + 5);
        assert!(BlockQueue::validate(&b, 20 * MIB + 5));
    }

    #[test]
    fn empty_file_is_complete() {
        let q = BlockQueue::new(0, 8 * MIB, MIB);
        assert!(q.is_complete());
    }

    #[test]
    fn spreads_connections_over_the_file() {
        let mut q = BlockQueue::new(64 * MIB, 8 * MIB, MIB);
        let a = q.next(N0).unwrap();
        // La primera pide desde el principio hasta el final del archivo.
        assert_eq!((a.start, a.end, a.request_end), (0, 8 * MIB, 64 * MIB));
        // La segunda empieza a mitad del tramo que queda.
        let b = q.next(N0).unwrap();
        assert_eq!((b.start, b.request_end), (32 * MIB, 64 * MIB));
        // La tercera, a mitad del tramo pendiente más grande.
        let c = q.next(N0).unwrap();
        assert_eq!(c.start, 16 * MIB);
        assert_eq!(c.request_end, 32 * MIB);
    }

    #[test]
    fn extends_into_next_block_with_same_request() {
        let mut q = BlockQueue::new(24 * MIB, 8 * MIB, MIB);
        let a = q.next(N0).unwrap();
        assert_eq!(q.commit(&a, 8 * MIB).1, Commit::Stop);
        let a2 = q.extend(&a, N0).unwrap();
        assert_eq!(
            (a2.start, a2.end, a2.request_end),
            (8 * MIB, 16 * MIB, 24 * MIB)
        );
        // Si el siguiente ya lo tiene otra conexión, no se extiende.
        let b = q.next(N0).unwrap();
        assert_eq!(b.start, 16 * MIB);
        q.commit(&a2, 16 * MIB);
        assert!(q.extend(&a2, N0).is_none());
    }

    #[test]
    fn commit_and_finish() {
        let mut q = BlockQueue::new(8 * MIB, 8 * MIB, MIB);
        let l = q.next(N0).unwrap();
        assert_eq!(
            q.commit(&l, 3 * MIB),
            (3 * MIB, Commit::Continue { end: 8 * MIB })
        );
        assert_eq!(q.commit(&l, 8 * MIB), (5 * MIB, Commit::Stop));
        assert!(q.is_complete());
        assert_eq!(q.written(), 8 * MIB);
    }

    #[test]
    fn released_block_resumes_from_written() {
        let mut q = BlockQueue::new(8 * MIB, 8 * MIB, MIB);
        let l = q.next(N0).unwrap();
        q.commit(&l, 2 * MIB);
        q.release(&l);
        let l2 = q.next(N0).unwrap();
        assert_eq!(l2.start, 2 * MIB);
    }

    #[test]
    fn splits_largest_active_block_when_nothing_pending() {
        let mut q = BlockQueue::new(8 * MIB, 8 * MIB, MIB);
        let a = q.next(N0).unwrap();
        q.commit(&a, 2 * MIB);
        let b = q.next(N0).unwrap();
        // Faltaban 6 MiB tras la posición 2 MiB: se parte en 5 MiB.
        assert_eq!((b.start, b.end), (5 * MIB, 8 * MIB));
        assert_eq!(q.allowed_end(&a), Some(5 * MIB));
        // La primera conexión termina en el nuevo final.
        assert_eq!(q.commit(&a, 6 * MIB).1, Commit::Stop);
        assert_eq!(q.commit(&b, 8 * MIB).1, Commit::Stop);
        assert!(q.is_complete());
        assert!(BlockQueue::validate(&q.snapshot(), 8 * MIB));
    }

    #[test]
    fn races_when_block_too_small_to_split() {
        let mut q = BlockQueue::new(MIB + 10, 8 * MIB, MIB);
        let a = q.next(N0).unwrap();
        q.commit(&a, 100);
        let b = q.next(N1).unwrap();
        assert_eq!(b.block, a.block);
        assert_eq!(b.start, 100);
        // Gana b; a recibe la orden de parar.
        assert_eq!(q.commit(&b, MIB + 10).1, Commit::Stop);
        assert_eq!(q.allowed_end(&a), None);
        assert_eq!(q.commit(&a, 200), (0, Commit::Stop));
        assert_eq!(q.written(), MIB + 10);
        // Como mucho dos conexiones por bloque.
        assert!(q.next(N0).is_none());
    }

    #[test]
    fn race_limit_per_block() {
        let mut q = BlockQueue::new(MIB, 8 * MIB, MIB);
        let _a = q.next(N0).unwrap();
        let _b = q.next(N0).unwrap();
        assert!(q.next(N0).is_none());
    }

    #[test]
    fn prefers_racing_a_block_from_another_network() {
        let mut q = BlockQueue::new(2 * MIB, MIB, MIB);
        let a = q.next(N0).unwrap();
        let b = q.next(N1).unwrap();
        assert_eq!((a.start, b.start), (0, MIB));
        q.commit(&a, 512 * 1024);
        q.commit(&b, MIB + 900 * 1024);
        // N0 debería ir a ayudar al bloque de N1, aunque le falte menos.
        let c = q.next(N0).unwrap();
        assert_eq!(c.block, b.block);
    }

    #[test]
    fn validate_detects_gaps() {
        let blocks = vec![
            Block {
                start: 0,
                end: 10,
                written: 0,
            },
            Block {
                start: 11,
                end: 20,
                written: 0,
            },
        ];
        assert!(!BlockQueue::validate(&blocks, 20));
    }
}
