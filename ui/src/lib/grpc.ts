const CODES = [
  'OK',
  'CANCELLED',
  'UNKNOWN',
  'INVALID_ARGUMENT',
  'DEADLINE_EXCEEDED',
  'NOT_FOUND',
  'ALREADY_EXISTS',
  'PERMISSION_DENIED',
  'RESOURCE_EXHAUSTED',
  'FAILED_PRECONDITION',
  'ABORTED',
  'OUT_OF_RANGE',
  'UNIMPLEMENTED',
  'INTERNAL',
  'UNAVAILABLE',
  'DATA_LOSS',
  'UNAUTHENTICATED',
];

export const GRPC_KINDS: Record<string, string> = {
  unary: 'unary',
  serverStreaming: 'server streaming',
  clientStreaming: 'client streaming',
  bidiStreaming: 'bidirectional',
};

/** The canonical name of a gRPC status code given as a number or a name. */
export function grpcCodeName(code: number | string | null | undefined): string {
  if (code == null) return 'OK';
  if (typeof code === 'number') return CODES[code] ?? String(code);
  return code.toUpperCase();
}
