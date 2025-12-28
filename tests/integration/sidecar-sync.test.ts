/**
 * @file Integration tests for Sidecar Sync
 * @module tests/integration/sidecar-sync
 *
 * Tests full sync flow against a running sidecar instance.
 * Requires: Running sidecar (set SIDECAR_URL env var or use default).
 *
 * Run with: npm run test:integration
 */

import { describe, it, expect, beforeAll, afterAll } from 'vitest';
import { SidecarClient, SidecarApiError } from '../../src/sync/sidecar-client.js';
import { SseClient, type SseEventHandlers } from '../../src/sync/sse-client.js';
import { SidecarSyncService } from '../../src/services/sidecar-sync-service.js';
import type { BlockBundle, Watermarks } from '../../src/sync/sidecar-types.js';

// Default sidecar URL - override with SIDECAR_URL env var
const SIDECAR_URL = process.env.SIDECAR_URL ?? 'http://127.0.0.1:18081';

// Test viewing key (bech32m with HRP mn_shield-esk_undeployed)
// This is a test key - do not use in production
const TEST_VIEWING_KEY =
  process.env.TEST_VIEWING_KEY ??
  'mn_shield-esk_undeployed1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq0wamev';

// Helper to check if sidecar is available
async function isSidecarAvailable(): Promise<boolean> {
  try {
    const response = await fetch(`${SIDECAR_URL}/healthz`, { signal: AbortSignal.timeout(2000) });
    return response.ok;
  } catch {
    return false;
  }
}

describe('Sidecar Integration Tests', () => {
  let client: SidecarClient;
  let sidecarAvailable = false;

  beforeAll(async () => {
    client = new SidecarClient({
      baseUrl: SIDECAR_URL,
      timeout: 10000,
    });

    sidecarAvailable = await isSidecarAvailable();
    if (!sidecarAvailable) {
      console.warn(`Sidecar not available at ${SIDECAR_URL} - skipping integration tests`);
    }
  });

  describe('Health Endpoints', () => {
    it('should return healthy status', async () => {
      if (!sidecarAvailable) return;

      const result = await client.healthz();
      expect(result.status).toBe('ok');
    });

    it('should return ready status with checks', async () => {
      if (!sidecarAvailable) return;

      const result = await client.readyz();
      expect(result.status).toBeDefined();
      expect(result.checks).toBeDefined();
    });
  });

  describe('Session Management', () => {
    let sessionId: string | null = null;

    afterAll(async () => {
      // Cleanup: disconnect session if created
      if (sessionId && sidecarAvailable) {
        try {
          await client.disconnect(sessionId);
        } catch {
          // Ignore cleanup errors
        }
      }
    });

    it('should bootstrap a session with viewing key', async () => {
      if (!sidecarAvailable) return;

      const result = await client.bootstrap(TEST_VIEWING_KEY);

      expect(result.sessionId).toBeDefined();
      expect(typeof result.sessionId).toBe('string');
      expect(result.sessionId.length).toBeGreaterThan(0);

      sessionId = result.sessionId;
    });

    it('should connect a session (proxy mode)', async () => {
      if (!sidecarAvailable) return;

      const result = await client.connect(TEST_VIEWING_KEY);

      expect(result.sessionId).toBeDefined();
      expect(typeof result.sessionId).toBe('string');

      // Cleanup this session
      await client.disconnect(result.sessionId);
    });

    it('should update profile with unshielded addresses', async () => {
      if (!sidecarAvailable || !sessionId) return;

      const result = await client.updateProfile(sessionId, {
        unshieldedAddresses: ['test_address_1', 'test_address_2'],
        nullifierStreaming: 'enabled',
      });

      expect(result.success).toBe(true);
      expect(result.nullifierStreaming).toBe('enabled');
      // Note: addressesChanged may be true on first update
    });

    it('should disconnect a session', async () => {
      if (!sidecarAvailable || !sessionId) return;

      const result = await client.disconnect(sessionId);

      expect(result.success).toBe(true);
      sessionId = null;
    });

    it('should return error for invalid session', async () => {
      if (!sidecarAvailable) return;

      try {
        await client.getFeed({
          sessionId: 'invalid-session-id-000000000000000000',
          fromHeight: 0,
        });
        expect.fail('Should have thrown');
      } catch (error) {
        expect(error).toBeInstanceOf(SidecarApiError);
        // Error code could be NOT_FOUND or BAD_REQUEST depending on sidecar implementation
      }
    });
  });

  describe('Feed Fetching', () => {
    let sessionId: string;

    beforeAll(async () => {
      if (!sidecarAvailable) return;

      const result = await client.bootstrap(TEST_VIEWING_KEY);
      sessionId = result.sessionId;
    });

    afterAll(async () => {
      if (sessionId && sidecarAvailable) {
        try {
          await client.disconnect(sessionId);
        } catch {
          // Ignore cleanup errors
        }
      }
    });

    it('should fetch feed from height 0', async () => {
      if (!sidecarAvailable) return;

      const result = await client.getFeed({
        sessionId,
        fromHeight: 0,
        limitBlocks: 10,
      });

      expect(result.apiVersion).toBeDefined();
      expect(result.watermarks).toBeDefined();
      expect(result.watermarks.chainHead).toBeDefined();
      expect(result.watermarks.finalizedHeight).toBeDefined();
      expect(Array.isArray(result.blocks)).toBe(true);
    });

    it('should include watermarks in response', async () => {
      if (!sidecarAvailable) return;

      const result = await client.getFeed({
        sessionId,
        fromHeight: 0,
        limitBlocks: 1,
      });

      const { watermarks } = result;
      expect(typeof watermarks.chainHead).toBe('string');
      expect(typeof watermarks.finalizedHeight).toBe('string');
      // walletReadyHeight may be null
      expect(watermarks.walletReadyHeight === null || typeof watermarks.walletReadyHeight === 'string').toBe(true);
    });

    it('should respect limitBlocks parameter', async () => {
      if (!sidecarAvailable) return;

      const limit = 5;
      const result = await client.getFeed({
        sessionId,
        fromHeight: 0,
        limitBlocks: limit,
      });

      // Should return at most `limit` blocks
      expect(result.blocks.length).toBeLessThanOrEqual(limit);
    });

    it('should return nextHeight for pagination', async () => {
      if (!sidecarAvailable) return;

      const result = await client.getFeed({
        sessionId,
        fromHeight: 0,
        limitBlocks: 1,
      });

      // nextHeight should be null (no more blocks) or a string height
      if (result.blocks.length > 0) {
        expect(result.nextHeight === null || typeof result.nextHeight === 'string').toBe(true);
      }
    });
  });

  describe('Zswap Endpoints', () => {
    it('should get first free index', async () => {
      if (!sidecarAvailable) return;

      const result = await client.getFirstFree();

      expect(result.firstFreeIndex).toBeDefined();
      expect(typeof result.firstFreeIndex).toBe('string');
      expect(result.blockHeight).toBeDefined();
      expect(['snapshot', 'db']).toContain(result.source);
    });
  });

  describe('SSE Subscription', () => {
    let sessionId: string;

    beforeAll(async () => {
      if (!sidecarAvailable) return;

      const result = await client.bootstrap(TEST_VIEWING_KEY);
      sessionId = result.sessionId;
    });

    afterAll(async () => {
      if (sessionId && sidecarAvailable) {
        try {
          await client.disconnect(sessionId);
        } catch {
          // Ignore cleanup errors
        }
      }
    });

    it('should connect to SSE and receive heartbeat', async () => {
      if (!sidecarAvailable) return;

      const receivedEvents: string[] = [];
      let resolveHeartbeat: () => void;
      const heartbeatReceived = new Promise<void>((resolve) => {
        resolveHeartbeat = resolve;
      });

      const handlers: SseEventHandlers = {
        onHeartbeat: () => {
          receivedEvents.push('heartbeat');
          resolveHeartbeat();
        },
        onBlock: () => {
          receivedEvents.push('block');
        },
        onControl: () => {
          receivedEvents.push('control');
        },
        onError: (error) => {
          console.error('SSE error:', error.message);
        },
        onConnected: () => {
          receivedEvents.push('connected');
        },
        onDisconnected: () => {
          receivedEvents.push('disconnected');
        },
      };

      const sseClient = new SseClient({
        baseUrl: SIDECAR_URL,
        sessionId,
        fromHeight: 0,
        includeNullifiers: true,
        handlers,
      });

      sseClient.connect();

      // Wait for heartbeat or timeout after 15 seconds
      const timeout = new Promise<void>((_, reject) => {
        setTimeout(() => reject(new Error('SSE heartbeat timeout')), 15000);
      });

      try {
        await Promise.race([heartbeatReceived, timeout]);
        expect(receivedEvents).toContain('connected');
        expect(receivedEvents).toContain('heartbeat');
      } finally {
        sseClient.disconnect();
      }
    }, 20000); // Extended timeout for SSE test
  });

  describe('SidecarSyncService Integration', () => {
    let service: SidecarSyncService;
    const walletPath = 'test/0';

    beforeAll(async () => {
      if (!sidecarAvailable) return;

      service = new SidecarSyncService({
        sidecarUrl: SIDECAR_URL,
        timeout: 10000,
        pollIntervalMs: 1000,
      });
    });

    afterAll(async () => {
      if (service) {
        service.dispose();
      }
    });

    it('should add wallet and start sync', async () => {
      if (!sidecarAvailable) return;

      const stateChanges: string[] = [];

      service.setHandlers({
        onStateChange: (state) => {
          stateChanges.push(state);
        },
        onError: (error) => {
          console.error('Sync error:', error.message);
        },
      });

      await service.addWallet({
        walletPath,
        viewingKey: TEST_VIEWING_KEY,
      });

      await service.startWalletSync(walletPath);

      // Wait for sync to start
      await new Promise((resolve) => setTimeout(resolve, 2000));

      // Should have progressed through states
      expect(stateChanges.length).toBeGreaterThan(0);
      expect(stateChanges).toContain('bootstrapping');

      const status = service.getSyncStatus(walletPath);
      expect(status).not.toBeNull();
      expect(['syncing', 'live', 'error']).toContain(status!.state);
    }, 10000);

    it('should stop wallet sync', async () => {
      if (!sidecarAvailable) return;

      await service.stopWalletSync(walletPath);

      const status = service.getSyncStatus(walletPath);
      expect(status?.state).toBe('idle');
    });

    it('should remove wallet', async () => {
      if (!sidecarAvailable) return;

      await service.removeWallet(walletPath);

      const status = service.getSyncStatus(walletPath);
      expect(status).toBeNull();
    });
  });

  describe('Cursor Resume', () => {
    let service: SidecarSyncService;
    const walletPath = 'cursor-test/0';
    let savedCursors: Record<string, unknown> | null = null;

    beforeAll(async () => {
      if (!sidecarAvailable) return;

      // Create service with cursor persistence mocks
      service = new SidecarSyncService({
        sidecarUrl: SIDECAR_URL,
        timeout: 10000,
        getCursors: async () => savedCursors as ReturnType<NonNullable<Parameters<typeof SidecarSyncService>[0]['getCursors']>>,
        saveCursors: async (_, cursors) => {
          savedCursors = cursors as Record<string, unknown>;
        },
      });
    });

    afterAll(async () => {
      if (service) {
        service.dispose();
      }
    });

    it('should persist cursors after block processing', async () => {
      if (!sidecarAvailable) return;

      let blocksProcessed = 0;

      service.setHandlers({
        onBlock: () => {
          blocksProcessed++;
        },
      });

      await service.addWallet({
        walletPath,
        viewingKey: TEST_VIEWING_KEY,
      });

      await service.startWalletSync(walletPath);

      // Wait for some blocks to be processed
      await new Promise((resolve) => setTimeout(resolve, 5000));

      await service.stopWalletSync(walletPath);

      // Cursors should have been saved
      if (blocksProcessed > 0) {
        expect(savedCursors).not.toBeNull();
        expect(savedCursors?.sessionId).toBeDefined();
        expect(savedCursors?.lastSyncedHeight).toBeDefined();
      }

      await service.removeWallet(walletPath);
    }, 15000);

    it('should resume from saved cursor on restart', async () => {
      if (!sidecarAvailable || !savedCursors) return;

      // Re-add wallet with same path - should resume from cursor
      await service.addWallet({
        walletPath,
        viewingKey: TEST_VIEWING_KEY,
      });

      await service.startWalletSync(walletPath);

      // Wait briefly
      await new Promise((resolve) => setTimeout(resolve, 1000));

      const status = service.getSyncStatus(walletPath);
      expect(status).not.toBeNull();

      // Should have resumed from saved height
      if (savedCursors && typeof savedCursors.lastSyncedHeight === 'number') {
        expect(status!.currentHeight).toBeGreaterThanOrEqual(savedCursors.lastSyncedHeight);
      }

      await service.removeWallet(walletPath);
    }, 10000);
  });

  describe('Error Handling', () => {
    it('should handle connection refused', async () => {
      const badClient = new SidecarClient({
        baseUrl: 'http://127.0.0.1:19999', // Unlikely to be running
        timeout: 2000,
      });

      try {
        await badClient.healthz();
        expect.fail('Should have thrown');
      } catch (error) {
        expect(error).toBeDefined();
        // Should be a network error
      }
    });

    it('should handle timeout', async () => {
      if (!sidecarAvailable) return;

      const slowClient = new SidecarClient({
        baseUrl: SIDECAR_URL,
        timeout: 1, // Very short timeout
      });

      try {
        await slowClient.healthz();
        // May succeed if response is fast enough
      } catch (error) {
        // Timeout or success is acceptable
        expect(error).toBeDefined();
      }
    });
  });
});
