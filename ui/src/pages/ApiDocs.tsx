import { Alert, Button, Stack, useComputedColorScheme } from '@mantine/core';
import { IconAlertTriangle, IconExternalLink } from '@tabler/icons-react';
import { useEffect, useRef, useState } from 'react';
import 'swagger-ui-dist/swagger-ui.css';
import { CSRF_HEADER, tokenStore } from '../api/http';
import { sameOrigin } from '../lib/origin';
import { Copy, PageHeader } from '../components/common';
import { LoadingState } from '../components/States';
import classes from './ApiDocs.module.css';

/** The class on the html element that switches Swagger UI to its own dark styles. */
const SWAGGER_DARK = 'dark-mode';

/** Keeps Swagger UI's dark mode in step with the page's colour scheme while the screen is open. */
function useSwaggerScheme() {
  const scheme = useComputedColorScheme('dark');
  useEffect(() => {
    const root = document.documentElement;
    root.classList.toggle(SWAGGER_DARK, scheme === 'dark');
    return () => root.classList.remove(SWAGGER_DARK);
  }, [scheme]);
}

export default function ApiDocs() {
  const host = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<'loading' | 'ready' | 'error'>('loading');
  useSwaggerScheme();

  useEffect(() => {
    let cancelled = false;
    void import('swagger-ui-dist/swagger-ui-es-bundle.js')
      .then((m) => {
        if (cancelled || !host.current) return;
        m.default({
          domNode: host.current,
          url: '/openapi.json',
          deepLinking: false,
          tryItOutEnabled: true,
          validatorUrl: null,
          persistAuthorization: false,
          displayRequestDuration: true,
          filter: true,
          docExpansion: 'list',
          defaultModelsExpandDepth: 0,
          requestInterceptor: (req) => {
            const token = tokenStore.get();
            const own = sameOrigin(req.url, window.location.href);
            if (token && own && !req.headers.Authorization) req.headers.Authorization = `Bearer ${token}`;
            if (!token && own && tokenStore.mode() === 'session') req.headers[CSRF_HEADER] = '1';
            return req;
          },
        });
        setState('ready');
      })
      .catch(() => {
        if (!cancelled) setState('error');
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const specUrl = `${window.location.origin}/openapi.json`;
  return (
    <Stack gap="md">
      <PageHeader
        title="API docs"
        description="The admin API, described by OpenAPI. “Try it out” sends requests with your current sign-in."
        meta={
          <>
            <span className="fm-chip">OpenAPI</span>
            <span className="fm-chip fm-chip-soft">try it out on</span>
            <span className="fm-chip">{specUrl}</span>
            <Copy value={specUrl} label="Copy the specification URL" />
          </>
        }
        actions={
          <Button component="a" href="/openapi.json" target="_blank" rel="noopener noreferrer" variant="default" rightSection={<IconExternalLink size={14} />}>
            openapi.json
          </Button>
        }
      />
      {state === 'loading' && <LoadingState label="Loading Swagger UI…" />}
      {state === 'error' && (
        <Alert color="red" icon={<IconAlertTriangle size={18} />} title="Swagger UI could not be loaded">
          Reload the page; the specification itself stays available at /openapi.json.
        </Alert>
      )}
      <div ref={host} className={classes.host} data-testid="swagger-host" />
    </Stack>
  );
}
