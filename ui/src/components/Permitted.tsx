import { Tooltip } from '@mantine/core';
import { cloneElement, type ReactElement } from 'react';
import { can as allowed, missingText, type Permission, type Where } from '../auth/permissions';
import { useAuth, useCan } from '../auth/useAuth';

/**
 * Renders the control as is when the principal holds `perm` (in `where`, see `useCan`; with `anywhere`, in
 * any space or globally), otherwise disabled inside a tooltip that names the missing permission, or nothing
 * with `hide`.
 */
export function Permitted({
  perm,
  where,
  anywhere,
  hide,
  children,
}: {
  perm: Permission;
  where?: Where;
  anywhere?: boolean;
  hide?: boolean;
  children: ReactElement<{ disabled?: boolean }>;
}) {
  const can = useCan();
  const { permissions } = useAuth();
  const why = anywhere ? (allowed(permissions, perm) ? null : missingText(perm)) : can.missing(perm, where);
  if (!why) return children;
  if (hide) return null;
  return (
    <Denied perm={perm} why={why}>
      {children}
    </Denied>
  );
}

/** The control disabled inside a tooltip that says why: for checks `Permitted` cannot express. */
export function Denied({ perm, why, children }: { perm: Permission; why: string; children: ReactElement<{ disabled?: boolean }> }) {
  return (
    <Tooltip label={why} withArrow multiline maw={280} events={{ hover: true, focus: true, touch: true }}>
      <span className="permitted" tabIndex={0} data-testid="missing-permission" data-permission={perm}>
        {cloneElement(children, { disabled: true })}
      </span>
    </Tooltip>
  );
}
