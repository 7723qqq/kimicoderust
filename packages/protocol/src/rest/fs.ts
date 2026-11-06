/**
 *   POST /v1/sessions/{sid}/fs:list
 *     Body: FsListRequest
 *     Response data: FsListResponse  { items, children_by_path?, truncated }
 *     Errors: 40401, 40409, 41304, 41303
 *
 *   POST /v1/sessions/{sid}/fs:read
 *     Body: FsReadRequest
 *     Response data: FsReadResponse
 *     Errors: 40401, 40409, 40906, 40907, 41302, 41304
 *   POST /v1/sessions/{sid}/fs:list_many
 *     Body: FsListManyRequest  (paths[] up to 100)
 *     Response data: FsListManyResponse  { results, truncated_paths?,
 *                                          partial_errors? }
 *
 *   POST /v1/sessions/{sid}/fs:stat
 *     Body: FsStatRequest
 *     Response data: FsEntry
 *     Errors: 40401, 40409, 41304
 *
 *   POST /v1/sessions/{sid}/fs:stat_many
 *     Body: FsStatManyRequest  (paths[] up to 1000)
 *     Response data: FsStatManyResponse  { entries: { [path]: FsEntry | null } }
 *     Per-path failures land as `null` (REST.md §3.9 line 524); only
 *     path-safety (41304) fails the whole call.
 *   POST /v1/sessions/{sid}/fs:search
 *     Body: FsSearchRequest   { query, limit?, include_globs?, exclude_globs?,
 *                               follow_gitignore? }
 *     Response data: FsSearchResponse  { items, truncated }
 *     An empty query lists the workspace root's top-level entries (dirs
 *     first) instead of fuzzy-matching.
 *     Errors: 40401, 41303, 41304
 *
 *   POST /v1/sessions/{sid}/fs:grep
 *     Body: FsGrepRequest
 *     Response data: FsGrepResponse  { files, files_scanned, truncated,
 *                                      elapsed_ms }
 *     Errors: 40401, 41303, 41304, 41305 (>30s grep timeout)
 *   POST /v1/sessions/{sid}/fs:git_status
 *     Body: FsGitStatusRequest  { root?, paths?, include_pull_request? }
 *     Response data: FsGitStatusResponse  { branch, ahead, behind, entries,
 *                                           additions, deletions }
 *     Errors: 40401, 40908 (not a git repo), 41304
 *
 *   POST /v1/sessions/{sid}/fs:git_status:batch
 *     Body: FsGitStatusBatchRequest  { roots (1..32), include_pull_request? }
 *     Response data: FsGitStatusBatchResponse  { results, errors }
 *
 *   POST /v1/sessions/{sid}/fs:pull_request
 *     Body: FsPullRequestRequest  { root? }
 *     Response data: FsPullRequest | null
 *
 *   POST /v1/sessions/{sid}/fs:pull_request:batch
 *     Body: FsPullRequestBatchRequest  { roots (1..32) }
 *     Response data: FsPullRequestBatchResponse  { results, errors }
 *
 *   POST /v1/sessions/{sid}/fs:diff
 *     Body: FsDiffRequest  { path, root? }
 *     Response data: FsDiffResponse  { path, diff, truncated }
 *     Errors: 40401, 40409, 40908 (not a git repo), 41304
 *
 *   GET /v1/sessions/{sid}/fs/{path}:download
 *     Response: binary stream or envelope (40401 / 40409 / 41304)
 *
 *   POST /v1/sessions/{sid}/fs:open
 *     Body: FsOpenRequest
 *     Response data: FsOpenResponse
 *
 *   POST /v1/sessions/{sid}/fs:reveal
 *     Body: FsRevealRequest
 *     Response data: FsRevealResponse
 *
 *   POST /v1/sessions/{sid}/fs:mkdir
 *     Body: FsMkdirRequest  { path, recursive? }
 *     Response data: FsEntry (the created directory)
 *     Errors: 40401, 40409 (parent missing), 40919 (already exists),
 *             41304 (path escapes cwd)
 */

import { z } from 'zod';

import {
  fsEntrySchema,
  fsGitStatusSchema,
  fsGrepFileHitSchema,
  fsSearchHitSchema,
  fsSuggestItemSchema,
} from '../fs';

export const fsListSortSchema = z.enum([
  'type_first',
  'name_asc',
  'name_desc',
  'mtime_desc',
  'size_desc',
]);
export type FsListSort = z.infer<typeof fsListSortSchema>;

export const fsListRequestSchema = z.object({
  path: z.string().default('.'),
  depth: z.number().int().min(1).max(10).default(1),
  limit: z.number().int().min(1).max(1000).default(200),
  show_hidden: z.boolean().default(false),
  follow_gitignore: z.boolean().default(true),
  exclude_globs: z.array(z.string()).optional(),
  // Globs to show even when gitignored (e.g. `.tmp`). Ignored ancestor
  // directories of a match stay reachable; allowing dot-paths additionally
  // requires `show_hidden: true` (v2 #4058).
  allow_ignored_globs: z.array(z.string()).optional(),
  // Follow symlinked directories whose targets are outside the workspace
  // (v2 #4457). Default `false`.
  allow_external_symlinks: z.boolean().optional(),
  sort: fsListSortSchema.default('type_first'),
  include_git_status: z.boolean().default(false),
});
export type FsListRequest = z.infer<typeof fsListRequestSchema>;

export const fsListResponseSchema = z.object({
  items: z.array(fsEntrySchema),
  children_by_path: z.record(z.string(), z.array(fsEntrySchema)).optional(),
  truncated: z.boolean(),
});
export type FsListResponse = z.infer<typeof fsListResponseSchema>;

export const fsReadEncodingRequestSchema = z.enum(['auto', 'utf-8', 'base64']);
export const fsReadEncodingResponseSchema = z.enum(['utf-8', 'base64']);
export type FsReadEncoding = z.infer<typeof fsReadEncodingResponseSchema>;

export const fsReadRequestSchema = z.object({
  path: z.string().min(1),
  offset: z.number().int().nonnegative().default(0),
  length: z.number().int().min(1).max(10_485_760).default(1_048_576),
  encoding: fsReadEncodingRequestSchema.default('auto'),
  // Allow reading through symlinks whose targets are outside the workspace
  // (v2 #4457). Default `false`.
  allow_external_symlinks: z.boolean().optional(),
});
export type FsReadRequest = z.infer<typeof fsReadRequestSchema>;

export const fsReadResponseSchema = z.object({
  path: z.string(),
  content: z.string(),
  encoding: fsReadEncodingResponseSchema,
  size: z.number().int().nonnegative(),
  truncated: z.boolean(),
  etag: z.string(),
  mime: z.string(),
  language_id: z.string().optional(),
  line_count: z.number().int().nonnegative().optional(),
  is_binary: z.boolean(),
});
export type FsReadResponse = z.infer<typeof fsReadResponseSchema>;

export const fsOpenRequestSchema = z.object({
  path: z.string().min(1),
  line: z.number().int().positive().optional(),
});
export type FsOpenRequest = z.infer<typeof fsOpenRequestSchema>;

export const fsOpenResponseSchema = z.object({
  opened: z.literal(true),
});
export type FsOpenResponse = z.infer<typeof fsOpenResponseSchema>;

export const fsRevealRequestSchema = z.object({
  path: z.string().min(1),
});
export type FsRevealRequest = z.infer<typeof fsRevealRequestSchema>;

export const fsRevealResponseSchema = z.object({
  revealed: z.literal(true),
});
export type FsRevealResponse = z.infer<typeof fsRevealResponseSchema>;

export const fsMkdirRequestSchema = z.object({
  path: z.string().min(1),
  recursive: z.boolean().default(false),
});
export type FsMkdirRequest = z.infer<typeof fsMkdirRequestSchema>;

export const fsMkdirResponseSchema = fsEntrySchema;
export type FsMkdirResponse = z.infer<typeof fsMkdirResponseSchema>;

export const fsOpenInAppIdSchema = z.enum([
  'finder',
  'cursor',
  'vscode',
  'iterm',
  'terminal',
]);
export type FsOpenInAppId = z.infer<typeof fsOpenInAppIdSchema>;

export const fsOpenInRequestSchema = z.object({
  app_id: fsOpenInAppIdSchema,
  path: z.string().min(1),
  line: z.number().int().positive().optional(),
});
export type FsOpenInRequest = z.infer<typeof fsOpenInRequestSchema>;

export const fsOpenInResponseSchema = z.object({
  opened: z.literal(true),
});
export type FsOpenInResponse = z.infer<typeof fsOpenInResponseSchema>;

export const fsListManyRequestSchema = z.object({
  paths: z.array(z.string().min(1)).min(1).max(100),
  depth: z.number().int().min(1).max(10).default(1),
  limit: z.number().int().min(1).max(1000).default(200),
  show_hidden: z.boolean().default(false),
  follow_gitignore: z.boolean().default(true),
  exclude_globs: z.array(z.string()).optional(),
  sort: fsListSortSchema.default('type_first'),
  include_git_status: z.boolean().default(false),
});
export type FsListManyRequest = z.infer<typeof fsListManyRequestSchema>;

export const fsListManyPartialErrorSchema = z.object({
  code: z.number().int(),
  msg: z.string(),
});
export type FsListManyPartialError = z.infer<typeof fsListManyPartialErrorSchema>;

export const fsListManyResponseSchema = z.object({
  results: z.record(z.string(), z.array(fsEntrySchema)),
  truncated_paths: z.array(z.string()).optional(),
  partial_errors: z.record(z.string(), fsListManyPartialErrorSchema).optional(),
});
export type FsListManyResponse = z.infer<typeof fsListManyResponseSchema>;

export const fsStatRequestSchema = z.object({
  path: z.string().min(1),
});
export type FsStatRequest = z.infer<typeof fsStatRequestSchema>;

export const fsStatResponseSchema = fsEntrySchema;
export type FsStatResponse = z.infer<typeof fsStatResponseSchema>;

export const fsStatManyRequestSchema = z.object({
  paths: z.array(z.string().min(1)).min(1).max(1000),
});
export type FsStatManyRequest = z.infer<typeof fsStatManyRequestSchema>;

export const fsStatManyResponseSchema = z.object({
  entries: z.record(z.string(), fsEntrySchema.nullable()),
});
export type FsStatManyResponse = z.infer<typeof fsStatManyResponseSchema>;

export const fsSearchRequestSchema = z.object({
  // An empty query is allowed: the server then lists the workspace root's
  // top-level entries (dirs first) instead of fuzzy-matching — the starting
  // set for @-mention style file pickers.
  query: z.string(),
  limit: z.number().int().min(1).max(200).default(50),
  include_globs: z.array(z.string()).optional(),
  exclude_globs: z.array(z.string()).optional(),
  follow_gitignore: z.boolean().default(true),
});
export type FsSearchRequest = z.infer<typeof fsSearchRequestSchema>;

export const fsSearchResponseSchema = z.object({
  items: z.array(fsSearchHitSchema),
  truncated: z.boolean(),
});
export type FsSearchResponse = z.infer<typeof fsSearchResponseSchema>;

export const fsSuggestRequestSchema = z.object({
  query: z.string(),
  limit: z.number().int().min(1).max(200).default(50),
  follow_gitignore: z.boolean().default(true),
  show_hidden: z.boolean().default(false),
  include_globs: z.array(z.string()).optional(),
  exclude_globs: z.array(z.string()).optional(),
});
export type FsSuggestRequest = z.infer<typeof fsSuggestRequestSchema>;

export const fsRootSuggestRequestSchema = fsSuggestRequestSchema.extend({
  roots: z.array(z.string().min(1)).min(1).max(32),
  runtime_id: z.string().min(1).optional(),
});
export type FsRootSuggestRequest = z.infer<typeof fsRootSuggestRequestSchema>;

export const fsSuggestResponseSchema = z.object({
  items: z.array(fsSuggestItemSchema),
  truncated: z.boolean(),
});
export type FsSuggestResponse = z.infer<typeof fsSuggestResponseSchema>;

export const fsGrepRequestSchema = z.object({
  pattern: z.string().min(1),
  regex: z.boolean().default(false),
  case_sensitive: z.boolean().default(true),
  include_globs: z.array(z.string()).optional(),
  exclude_globs: z.array(z.string()).optional(),
  follow_gitignore: z.boolean().default(true),
  max_files: z.number().int().min(1).max(10_000).default(200),
  max_matches_per_file: z.number().int().min(1).max(10_000).default(50),
  max_total_matches: z.number().int().min(1).max(100_000).default(5000),
  context_lines: z.number().int().min(0).max(10).default(2),
});
export type FsGrepRequest = z.infer<typeof fsGrepRequestSchema>;

export const fsGrepResponseSchema = z.object({
  files: z.array(fsGrepFileHitSchema),
  files_scanned: z.number().int().nonnegative(),
  truncated: z.boolean(),
  elapsed_ms: z.number().int().nonnegative(),
});
export type FsGrepResponse = z.infer<typeof fsGrepResponseSchema>;

export const fsGitStatusRequestSchema = z.object({
  // Folder scope: run the query at this workspace-relative directory instead
  // of the session root; entries come back relative to it (v2 #4527).
  root: z.string().min(1).optional(),
  paths: z.array(z.string().min(1)).optional(),
  // Whether the response should look up the branch's pull request. Default
  // `true`; the fork's server never performs the lookup either way.
  include_pull_request: z.boolean().optional(),
});
export type FsGitStatusRequest = z.infer<typeof fsGitStatusRequestSchema>;

export const fsPullRequestSchema = z.object({
  number: z.number().int().positive(),
  state: z.enum(['open', 'merged', 'closed', 'draft']),
  url: z.string().url(),
});
export type FsPullRequest = z.infer<typeof fsPullRequestSchema>;

export const fsGitStatusResponseSchema = z.object({
  branch: z.string(),
  ahead: z.number().int().nonnegative(),
  behind: z.number().int().nonnegative(),
  entries: z.record(z.string(), fsGitStatusSchema),
  // Aggregate working-tree diff against HEAD (`git diff --numstat HEAD`):
  // summed added/deleted lines across all changed files. Binary files (numstat
  // `-`) contribute 0. Both 0 for a clean tree or a repo with no commits yet.
  additions: z.number().int().nonnegative(),
  deletions: z.number().int().nonnegative(),
  // GitHub pull request for the current branch, looked up via `gh pr view`.
  // Null when not a GitHub repo, `gh` is unavailable/unauthenticated, the
  // branch has no PR, or the lookup failed/timed out. Never fails the request.
  pullRequest: fsPullRequestSchema.nullable(),
});
export type FsGitStatusResponse = z.infer<typeof fsGitStatusResponseSchema>;

export const fsDiffRequestSchema = z.object({
  path: z.string().min(1),
  // Folder scope for the diff (v2 #4527), resolved like git_status's `root`.
  root: z.string().min(1).optional(),
});
export type FsDiffRequest = z.infer<typeof fsDiffRequestSchema>;

export const fsDiffResponseSchema = z.object({
  path: z.string(),
  diff: z.string(),
  truncated: z.boolean(),
});
export type FsDiffResponse = z.infer<typeof fsDiffResponseSchema>;

export const fsBatchErrorSchema = z.object({
  code: z.number().int(),
  msg: z.string(),
});
export type FsBatchError = z.infer<typeof fsBatchErrorSchema>;

// v2 #4527 batched folder git queries.
export const fsGitStatusBatchRequestSchema = z.object({
  roots: z.array(z.string().min(1)).min(1).max(32),
  include_pull_request: z.boolean().optional(),
});
export type FsGitStatusBatchRequest = z.infer<typeof fsGitStatusBatchRequestSchema>;

export const fsGitStatusBatchResponseSchema = z.object({
  results: z.record(z.string(), fsGitStatusResponseSchema),
  errors: z.record(z.string(), fsBatchErrorSchema),
});
export type FsGitStatusBatchResponse = z.infer<typeof fsGitStatusBatchResponseSchema>;

export const fsPullRequestRequestSchema = z.object({
  root: z.string().min(1).optional(),
});
export type FsPullRequestRequest = z.infer<typeof fsPullRequestRequestSchema>;

export const fsPullRequestBatchRequestSchema = z.object({
  roots: z.array(z.string().min(1)).min(1).max(32),
});
export type FsPullRequestBatchRequest = z.infer<typeof fsPullRequestBatchRequestSchema>;

export const fsPullRequestBatchResponseSchema = z.object({
  results: z.record(z.string(), fsPullRequestSchema.nullable()),
  errors: z.record(z.string(), fsBatchErrorSchema),
});
export type FsPullRequestBatchResponse = z.infer<typeof fsPullRequestBatchResponseSchema>;

export const fsDownloadParamsSchema = z.object({
  path: z.string().min(1),
  range: z.string().optional(),
  if_none_match: z.string().optional(),
});
export type FsDownloadParams = z.infer<typeof fsDownloadParamsSchema>;
