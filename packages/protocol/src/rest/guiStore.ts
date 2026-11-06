/**
 * GUI key-value store (mirrors the browser localStorage API for the Web UI /
 * Desktop hosts):
 *
 *   GET  /v1/gui/store/getItem        query: { key }      data: GuiStoreGetItemResponse
 *   POST /v1/gui/store/setItem        body: guiStoreSetItemBodySchema   data: {}
 *   POST /v1/gui/store/removeItem     body: guiStoreRemoveItemBodySchema data: {}
 *   POST /v1/gui/store/clear          -                     data: {}
 *   GET  /v1/gui/store/length         -                     data: GuiStoreLengthResponse
 */

import { z } from 'zod';

const keySchema = z.string().min(1).max(256);

export const guiStoreGetItemQuerySchema = z.object({ key: keySchema });

export const guiStoreSetItemBodySchema = z.object({
  key: keySchema,
  value: z.string(),
});

export const guiStoreRemoveItemBodySchema = z.object({ key: keySchema });

export const guiStoreGetItemResponseSchema = z.object({
  value: z.string().nullable(),
});
export type GuiStoreGetItemResponse = z.infer<typeof guiStoreGetItemResponseSchema>;

export const guiStoreLengthResponseSchema = z.object({
  length: z.number(),
});
export type GuiStoreLengthResponse = z.infer<typeof guiStoreLengthResponseSchema>;
