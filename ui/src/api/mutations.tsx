import { Text } from '@mantine/core';
import { modals } from '@mantine/modals';
import { notifications } from '@mantine/notifications';
import { useMutation, useQueryClient, type QueryKey } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { errorMessage } from './http';

export function notifySuccess(message: ReactNode, title?: string): void {
  notifications.show({ color: 'green', title, message, autoClose: 3500 });
}

export function notifyError(error: unknown, title = 'Action failed'): void {
  notifications.show({ color: 'red', title, message: errorMessage(error), autoClose: 8000 });
}

export interface ApiMutationOptions<TVars, TData> {
  fn: (vars: TVars) => Promise<TData>;
  invalidate?: QueryKey[];
  success?: string | ((data: TData, vars: TVars) => string | null);
  errorTitle?: string;
  onSuccess?: (data: TData, vars: TVars) => void;
}

export function useApiMutation<TVars = void, TData = unknown>(opts: ApiMutationOptions<TVars, TData>) {
  const qc = useQueryClient();
  return useMutation<TData, unknown, TVars>({
    mutationFn: opts.fn,
    onSuccess: async (data, vars) => {
      await Promise.all((opts.invalidate ?? []).map((k) => qc.invalidateQueries({ queryKey: k })));
      const msg = typeof opts.success === 'function' ? opts.success(data, vars) : opts.success;
      if (msg) notifySuccess(msg);
      opts.onSuccess?.(data, vars);
    },
    onError: (e) => notifyError(e, opts.errorTitle),
  });
}

export function confirmAction(opts: { title: string; message: ReactNode; confirm?: string; danger?: boolean; onConfirm: () => void }): void {
  modals.openConfirmModal({
    title: opts.title,
    children: typeof opts.message === 'string' ? <Text size="sm">{opts.message}</Text> : opts.message,
    labels: { confirm: opts.confirm ?? 'Confirm', cancel: 'Cancel' },
    confirmProps: { color: opts.danger === false ? undefined : 'red', 'data-testid': 'confirm-button' } as Record<string, unknown>,
    onConfirm: opts.onConfirm,
  });
}
