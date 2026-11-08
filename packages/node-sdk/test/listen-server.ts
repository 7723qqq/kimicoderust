import type { Server } from 'node:http';

/**
 * Bind `server` to an ephemeral loopback port and resolve with the port.
 *
 * A bare `server.listen(0, '127.0.0.1', resolve)` never settles when the bind
 * fails — a sandbox refusing the socket, an exhausted descriptor table under a
 * loaded suite — because the failure is emitted as an `error` event nobody
 * listens for. The test then hangs until the runner's timeout with no cause
 * attached, which is the "silently hangs" shape this helper removes.
 */
export function listenOnEphemeralPort(server: Server): Promise<number> {
  return new Promise((resolve, reject) => {
    const onError = (error: Error): void => reject(error);
    server.once('error', onError);
    server.listen(0, '127.0.0.1', () => {
      server.off('error', onError);
      const address = server.address();
      if (address === null || typeof address === 'string') {
        reject(new Error('listen() resolved without a TCP address'));
        return;
      }
      resolve(address.port);
    });
  });
}
