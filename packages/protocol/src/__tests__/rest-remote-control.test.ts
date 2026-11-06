import { describe, expect, it } from 'vitest';

import {
  remoteControlAlreadyRunningDetailsSchema,
  remoteControlConflictSchema,
  remoteControlDeviceListSchema,
  remoteControlDeviceMutationResponseSchema,
  remoteControlDeviceParamsSchema,
  remoteControlStatusSchema,
  renameRemoteControlDeviceRequestSchema,
} from '../rest/remoteControl';

const device = {
  device_id: 'device-1',
  alias: 'Workstation',
  status: 'online',
  platform: 'win32',
  client_version: 'kimi-code/1.0.0',
  local_base_url: 'http://127.0.0.1:3461',
  created_at: '2026-01-01T00:00:00.000Z',
  updated_at: '2026-01-01T00:00:00.000Z',
  last_remote_access_at: '2026-01-01T00:00:00.000Z',
} as const;

describe('remoteControlDeviceListSchema', () => {
  it('accepts the documented list and projects only device fields', () => {
    expect(
      remoteControlDeviceListSchema.parse({
        devices: [{ ...device, internal_note: 'cloud-only' }],
        max_devices: 3,
      }),
    ).toEqual({ devices: [device], max_devices: 3 });
  });

  it('rejects malformed devices and non-numeric max_devices', () => {
    expect(
      remoteControlDeviceListSchema.safeParse({
        devices: [{ ...device, status: 'busy' }],
      }).success,
    ).toBe(false);
    expect(
      remoteControlDeviceListSchema.safeParse({ devices: [{ alias: 'missing fields' }] }).success,
    ).toBe(false);
    expect(
      remoteControlDeviceListSchema.safeParse({ devices: [], max_devices: 'three' }).success,
    ).toBe(false);
  });
});

describe('remote control device mutation schemas', () => {
  it('requires a non-empty id and an exact ok response', () => {
    expect(remoteControlDeviceParamsSchema.parse({ id: 'device-1' })).toEqual({ id: 'device-1' });
    expect(remoteControlDeviceParamsSchema.safeParse({ id: '' }).success).toBe(false);
    expect(remoteControlDeviceMutationResponseSchema.parse({ ok: true })).toEqual({ ok: true });
    expect(remoteControlDeviceMutationResponseSchema.safeParse({ ok: false }).success).toBe(false);
  });

  it('trims the rename alias and enforces 1..=50 characters', () => {
    expect(renameRemoteControlDeviceRequestSchema.parse({ alias: '  Lab machine  ' })).toEqual({
      alias: 'Lab machine',
    });
    expect(renameRemoteControlDeviceRequestSchema.safeParse({ alias: '   ' }).success).toBe(false);
    expect(renameRemoteControlDeviceRequestSchema.safeParse({ alias: 'x'.repeat(51) }).success).toBe(
      false,
    );
    expect(renameRemoteControlDeviceRequestSchema.safeParse({ alias: 7 }).success).toBe(false);
  });
});

describe('remote control conflict schemas', () => {
  const holder = {
    pid: 4321,
    local_origin: 'http://127.0.0.1:3461',
    device_id: 'device-1',
    url: 'https://rc.example/device-1',
    started_at: 1_760_000_000_000,
  } as const;

  it('carries the holder on the status wire when a gate is held', () => {
    expect(
      remoteControlStatusSchema.parse({ enabled: false, state: 'off', conflict: holder }),
    ).toEqual({ enabled: false, state: 'off', conflict: holder });
    expect(remoteControlConflictSchema.parse(holder)).toEqual(holder);
  });

  it('rejects a holder missing the started_at timestamp', () => {
    const { started_at: _startedAt, ...partial } = holder;
    expect(remoteControlStatusSchema.safeParse({ enabled: false, state: 'off', conflict: partial })
      .success).toBe(false);
    expect(remoteControlAlreadyRunningDetailsSchema.parse(holder)).toEqual(holder);
    expect(
      remoteControlAlreadyRunningDetailsSchema.safeParse({ ...holder, pid: 'adapter' }).success,
    ).toBe(false);
  });
});
