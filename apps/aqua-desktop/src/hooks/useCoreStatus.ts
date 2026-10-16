import { useSyncExternalStore } from 'react';
import { coreConnected, describeStarting, fetchCoreStatus } from '../api/coreStatus';
import { createCoreStatusStore } from './coreStatusStore';

// One poller for the whole page (see coreStatusStore.ts), shared by every component that shows the core's state.
const store = createCoreStatusStore(fetchCoreStatus, coreConnected);

/**
 * The core's state for a component. `status` is the last good status, or `null` when the core is not up. While the core
 * answers "starting" (it has its port but is not ready), `status` is `null` too, so nothing shows empty engines as missing;
 * `starting` then says so, with the seconds, and is `null` at every other time.
 */
export function useCoreStatus() {
  const { status: answer, loading } = useSyncExternalStore(store.subscribe, store.getSnapshot);
  const starting = describeStarting(answer);
  const status = starting ? null : answer;
  return { status, loading, connected: coreConnected(status), starting };
}
