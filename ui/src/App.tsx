import { MantineProvider } from '@mantine/core';
import { ModalsProvider } from '@mantine/modals';
import { Notifications } from '@mantine/notifications';
import { QueryCache, QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { lazy, Suspense, useState } from 'react';
import { BrowserRouter, Navigate, Route, Routes, useLocation } from 'react-router';
import { ApiError } from './api/http';
import { AuthProvider } from './auth/AuthProvider';
import { SpaceProvider } from './auth/SpaceProvider';
import { useAuth } from './auth/useAuth';
import { Shell } from './components/Shell';
import { LoadingState } from './components/States';
import { LoginPage } from './pages/Login';
import { styleNonce } from './lib/nonce';
import { theme } from './theme';

const DashboardPage = lazy(() => import('./pages/Dashboard'));
const StubsPage = lazy(() => import('./pages/Stubs'));
const StubEditorPage = lazy(() => import('./pages/StubEditor'));
const JournalPage = lazy(() => import('./pages/Journal'));
const NearMissesPage = lazy(() => import('./pages/NearMisses'));
const VerifyPage = lazy(() => import('./pages/Verify'));
const ScenariosPage = lazy(() => import('./pages/Scenarios'));
const KvPage = lazy(() => import('./pages/Kv'));
const ChaosPage = lazy(() => import('./pages/Chaos'));
const CallbacksPage = lazy(() => import('./pages/Callbacks'));
const RecordingsPage = lazy(() => import('./pages/Recordings'));
const FilesPage = lazy(() => import('./pages/Files'));
const SettingsPage = lazy(() => import('./pages/Settings'));
const GrpcPage = lazy(() => import('./pages/Grpc'));
const AsyncPage = lazy(() => import('./pages/AsyncMocks'));
const ServicesPage = lazy(() => import('./pages/Services'));
const ContractTestsPage = lazy(() => import('./pages/ContractTests'));
const AuditPage = lazy(() => import('./pages/Audit'));
const TokensPage = lazy(() => import('./pages/Tokens'));
const AccessPage = lazy(() => import('./pages/Access'));
const ApiDocsPage = lazy(() => import('./pages/ApiDocs'));
const NotFoundPage = lazy(() => import('./pages/NotFound'));

function makeClient() {
  return new QueryClient({
    queryCache: new QueryCache(),
    defaultOptions: {
      queries: {
        retry: (count, err) => !(err instanceof ApiError && err.status >= 400 && err.status < 500) && count < 2,
        refetchOnWindowFocus: false,
        staleTime: 2_000,
      },
      mutations: { retry: false },
    },
  });
}

function Protected() {
  const { status } = useAuth();
  const loc = useLocation();
  if (status === 'checking') return <LoadingState label="Connecting…" />;
  if (status === 'anonymous') return <Navigate to="/login" replace state={{ from: loc.pathname + loc.search }} />;
  return (
    <Shell>
      <Suspense fallback={<LoadingState />}>
        <Routes>
          <Route index element={<DashboardPage />} />
          <Route path="stubs" element={<StubsPage />} />
          <Route path="stubs/new" element={<StubEditorPage />} />
          <Route path="stubs/:id" element={<StubEditorPage />} />
          <Route path="requests" element={<JournalPage />} />
          <Route path="near-misses" element={<NearMissesPage />} />
          <Route path="verify" element={<VerifyPage />} />
          <Route path="scenarios" element={<ScenariosPage />} />
          <Route path="kv" element={<KvPage />} />
          <Route path="chaos" element={<ChaosPage />} />
          <Route path="callbacks" element={<CallbacksPage />} />
          <Route path="recordings" element={<RecordingsPage />} />
          <Route path="files" element={<FilesPage />} />
          <Route path="settings" element={<SettingsPage />} />
          <Route path="grpc" element={<GrpcPage />} />
          <Route path="async" element={<AsyncPage />} />
          <Route path="services" element={<ServicesPage />} />
          <Route path="services/:id" element={<ServicesPage />} />
          <Route path="contract-tests" element={<ContractTestsPage />} />
          <Route path="audit" element={<AuditPage />} />
          <Route path="tokens" element={<TokensPage />} />
          <Route path="access" element={<AccessPage />} />
          <Route path="api" element={<ApiDocsPage />} />
          <Route path="*" element={<NotFoundPage />} />
        </Routes>
      </Suspense>
    </Shell>
  );
}

export function AppRoutes() {
  return (
    <Routes>
      <Route path="/login" element={<LoginPage />} />
      <Route path="/*" element={<Protected />} />
    </Routes>
  );
}

export function App() {
  const [client] = useState(makeClient);
  return (
    <MantineProvider theme={theme} defaultColorScheme="auto" withCssVariables={false} withGlobalClasses={false} getStyleNonce={() => styleNonce() ?? ''}>
      <QueryClientProvider client={client}>
        <ModalsProvider>
          <Notifications position="top-right" limit={4} />
          <BrowserRouter basename="/ui">
            <AuthProvider>
              <SpaceProvider>
                <AppRoutes />
              </SpaceProvider>
            </AuthProvider>
          </BrowserRouter>
        </ModalsProvider>
      </QueryClientProvider>
    </MantineProvider>
  );
}
