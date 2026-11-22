import { execFileSync } from 'node:child_process';
import { repoRoot } from './server';

// Builds the binaries the tests run, so a plain `npx playwright test` works.
export default function globalSetup(): void {
  execFileSync(
    'cargo',
    ['build', '--locked', '-p', 'factory-server', '--bin', 'factory-server', '--example', 'e2e_fixture'],
    { cwd: repoRoot, stdio: 'inherit' },
  );
}
