<?php
declare(strict_types=1);

/**
 * Nextcloud PHP bootstrap shim for FastCGI dispatch.
 *
 * PHP-FPM's SCRIPT_FILENAME is always set to this file by the Rust proxy.
 * The Rust server injects:
 *   HTTP_X_NC_USER          — authenticated UID (absent for unauthenticated requests)
 *   HTTP_X_NC_SESSION_TOKEN — raw bearer / app-token value
 *   HTTP_X_NC_IS_ADMIN      — "1" if the user is in the admin group, "0" otherwise
 *   NC_ORIGINAL_SCRIPT      — the original PHP entry-point path so the shim
 *                             can route to the right app
 *
 * Security model (§7.3):
 *   The Rust proxy strips any client-supplied X-NC-User header before forwarding,
 *   then re-injects HTTP_X_NC_USER only for requests that have passed Rust-side
 *   auth (token validation, brute-force check, 2FA gate).  A missing or empty
 *   HTTP_X_NC_USER therefore means the request arrived without going through Rust
 *   — i.e. someone connected directly to the FastCGI Unix socket.
 *
 * Bootstrap strategy (§7.4):
 *   1. reject_unauthenticated_shim_request() — security gate
 *   2. OC::init() — sets up the full DI container and services from config.php
 *   3. setVolatileActiveUser() — injects the pre-authenticated user into the
 *      session without touching PHP session state; OC::handleRequest() then sees
 *      isLoggedIn() === true and skips the PHP-side auth/login step entirely
 *   4. Route the request: for index.php / OCS entry points OC::handleRequest()
 *      is called; for remote.php service entry points the service is resolved
 *      and the target app file is included directly (mirrors remote.php logic).
 *
 * This file must NEVER be placed in a web-accessible directory.
 * The FastCGI socket must be chmod 0600, owned by the nc-server process user.
 * See docs/deployment.md for the full security requirements.
 */

// ── Security gate — must be the very first action ────────────────────────────
// Reject any FastCGI request that did not arrive through the Rust auth layer.
// This is the last line of defence; the primary control is the 0600 socket.
// Returns true when authenticated, false for whitelisted unauthenticated probes.
$_NC_IS_AUTHENTICATED = reject_unauthenticated_shim_request();

// ── Locate Nextcloud SERVERROOT ───────────────────────────────────────────────
// Shim lives at {NC_ROOT}/core-rs/php-shim/index.php, so NC_ROOT is two
// dirname() levels up.
$_NC_ROOT = dirname(__DIR__, 2);

// ── Load the Nextcloud framework ──────────────────────────────────────────────
// versioncheck.php only outputs a friendly error if PHP is too old, then dies.
// base.php bootstraps the full DI container (autoloaders, config, server boot).
// OC::init() inside base.php sets OC::$SERVERROOT from __DIR__ of base.php,
// which correctly yields $_NC_ROOT — no manual override needed.
require_once $_NC_ROOT . '/lib/versioncheck.php';
require_once $_NC_ROOT . '/lib/base.php';

OC::init();

// ── Inject pre-authenticated user (conditional) ────────────────────────────────
// When the request is authenticated Rust has already validated the
// token/password and brute-force gates before forwarding.  We use
// setVolatileActiveUser() so that:
//   - isLoggedIn() returns true (suppresses OC::handleRequest login attempt)
//   - no PHP session write-back occurs (this is a stateless API request)
//
// Unauthenticated whitelisted probes (e.g. the public capabilities fetch) skip
// this block so PHP sees no session and naturally calls getCapabilities(true),
// returning only IPublicCapability results.
if ($_NC_IS_AUTHENTICATED) {
    $_NC_UID = $_SERVER['HTTP_X_NC_USER'];
    $_NC_userManager = \OCP\Server::get(\OCP\IUserManager::class);
    $_NC_user = $_NC_userManager->get($_NC_UID);

    if ($_NC_user === null || !$_NC_user->isEnabled()) {
        http_response_code(403);
        header('Content-Type: text/plain; charset=UTF-8');
        echo "403 Forbidden: user '" . htmlspecialchars($_NC_UID, ENT_QUOTES | ENT_SUBSTITUTE, 'UTF-8') . "' not found or disabled\n";
        exit(0);
    }

    \OCP\Server::get(\OCP\IUserSession::class)->setVolatileActiveUser($_NC_user);
}

// ── Route request ─────────────────────────────────────────────────────────────
// Dispatch based on the original PHP entry-point path the Rust proxy recorded.
// NC_ORIGINAL_SCRIPT is the resolved filesystem path of the entry point
// (e.g. "/srv/nc/index.php" or "/srv/nc/remote.php").
$_NC_ORIGINAL_SCRIPT = $_SERVER['NC_ORIGINAL_SCRIPT'] ?? '';

// Normalise to the base filename so comparisons work regardless of install path.
$_NC_ENTRY = basename($_NC_ORIGINAL_SCRIPT);

switch ($_NC_ENTRY) {
    // ── index.php, OCS entry points, clean URLs ───────────────────────────────
    // OC::handleRequest() resolves the path through Symfony routing and
    // dispatches to the correct app controller.  Because setVolatileActiveUser()
    // was called above, handleRequest() skips the PHP-side login step entirely.
    case 'index.php':
    case 'v1.php':    // /ocs/v1.php/...
    case 'v2.php':    // /ocs/v2.php/...
        OC::handleRequest();
        break;

    // ── remote.php — DAV adjacent services (CalDAV, CardDAV, direct, …) ───────
    // The Rust native DAV layer handles WebDAV and DAV v2 directly; only
    // services not claimed by Rust (caldav, carddav, calendar, contacts,
    // direct) arrive here.  Mirror the service-resolution logic from
    // {NC_ROOT}/remote.php without re-bootstrapping the framework.
    case 'remote.php':
        route_remote_php($_NC_ROOT);
        break;

    // ── public.php — public share DAV  ────────────────────────────────────────
    // Mirrors public.php service-resolution logic.
    // Note: public share requests arrive here because the Rust router sends
    // /public.php/{*path} to the FastCGI fallback.
    case 'public.php':
        route_public_php($_NC_ROOT);
        break;

    // ── Fallback ──────────────────────────────────────────────────────────────
    // For any entry point not explicitly mapped above, attempt to handle via
    // the standard Nextcloud router.  This covers edge cases and newly added
    // entry points without requiring shim changes.
    default:
        OC::handleRequest();
        break;
}

exit(0);

// ─────────────────────────────────────────────────────────────────────────────
// Helper functions
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Validate that the request arrived through the Rust proxy layer.
 *
 * The Rust proxy injects HTTP_X_NC_PROXIED=1 on every proxied request
 * (§7.3 / §7.8) and strips any client-supplied X-NC-Proxied header before
 * forwarding.  Its presence is the channel trust signal — distinguishing
 * legitimate Rust-proxied requests from direct FastCGI socket connections
 * that bypass the Rust authentication layer.
 *
 * Allowed pass-through scenarios:
 *   - Authenticated requests (HTTP_X_NC_USER non-empty): Rust validated the
 *     token/password before forwarding.  setVolatileActiveUser() will be called
 *     so PHP sees a logged-in user and skips its own login step.
 *   - Unauthenticated proxied requests (HTTP_X_NC_USER absent): login flows
 *     (/login/flow, /login/v2/*), well-known redirects, public pages, the
 *     IPublicCapability capabilities probe, etc.  PHP runs its own auth
 *     naturally for these — no identity injection needed.
 *
 * The socket 0600 permission is the primary access control; this check is
 * defence-in-depth.
 *
 * @return bool  true when HTTP_X_NC_USER is non-empty (Rust injected an
 *               authenticated identity — caller should call setVolatileActiveUser);
 *               false when the request is legitimately unauthenticated (PHP
 *               handles auth itself).
 *               Never returns on rejection (calls exit(0)).
 */
function reject_unauthenticated_shim_request(): bool
{
    // Primary trust signal: HTTP_X_NC_PROXIED=1 is injected by the Rust proxy
    // on every request and stripped from any client-supplied X-NC-Proxied header.
    // A missing or incorrect value means the connection bypassed the Rust proxy
    // (direct FastCGI socket access).
    if (($_SERVER['HTTP_X_NC_PROXIED'] ?? '') !== '1') {
        // Do not use http_response_code() here — some SAPI implementations
        // buffer the status line; header() with the full Status pseudo-header
        // is the most portable way to emit it in a CGI context.
        header('Status: 403 Forbidden');
        header('Content-Type: text/plain; charset=UTF-8');
        echo "403 Forbidden: request did not pass Rust proxy layer\n";
        exit(0);
    }

    // Return true when the user identity was injected (authenticated request)
    // so the caller can invoke setVolatileActiveUser().
    // Return false for unauthenticated proxied requests so PHP handles auth naturally.
    $user = $_SERVER['HTTP_X_NC_USER'] ?? '';
    return $user !== '';
}

/**
 * Route a remote.php-destined request to the appropriate app file.
 *
 * Mirrors the service-resolution logic in {NC_ROOT}/remote.php but skips the
 * bootstrap (already done) and the auth stack (already bypassed via
 * setVolatileActiveUser).
 *
 * Built-in service map (covers PHP-FPM-served services; Rust handles webdav /
 * dav / files natively and those never reach this shim):
 *
 *   caldav    → dav/appinfo/v1/caldav.php
 *   calendar  → dav/appinfo/v1/caldav.php
 *   carddav   → dav/appinfo/v1/carddav.php
 *   contacts  → dav/appinfo/v1/carddav.php
 *   direct    → dav/appinfo/v2/direct.php
 *
 * Unknown services are looked up in oc_appconfig (core.remote_{service}).
 *
 * @param string $ncRoot Absolute path to the Nextcloud installation root.
 */
function route_remote_php(string $ncRoot): void
{
    header("Content-Security-Policy: default-src 'none';");

    $services = [
        'webdav'   => 'dav/appinfo/v1/webdav.php',
        'dav'      => 'dav/appinfo/v2/remote.php',
        'caldav'   => 'dav/appinfo/v1/caldav.php',
        'calendar' => 'dav/appinfo/v1/caldav.php',
        'carddav'  => 'dav/appinfo/v1/carddav.php',
        'contacts' => 'dav/appinfo/v1/carddav.php',
        'files'    => 'dav/appinfo/v1/webdav.php',
        'direct'   => 'dav/appinfo/v2/direct.php',
    ];

    $request = \OCP\Server::get(\OCP\IRequest::class);
    $pathInfo = $request->getPathInfo();
    if ($pathInfo === false || $pathInfo === '') {
        http_response_code(404);
        return;
    }

    if (!$pos = strpos((string)$pathInfo, '/', 1)) {
        $pos = strlen((string)$pathInfo);
    }
    $service = substr((string)$pathInfo, 1, $pos - 1);

    $file = $services[$service] ?? \OCP\Server::get(\OCP\IConfig::class)->getAppValue('core', 'remote_' . $service);
    if (!$file) {
        http_response_code(404);
        return;
    }

    $file = ltrim($file, '/');
    $parts = explode('/', $file, 2);
    $app = $parts[0];

    $appManager = \OCP\Server::get(\OCP\App\IAppManager::class);
    \OC::$REQUESTEDAPP = $app;
    $appManager->loadApps(['authentication']);
    $appManager->loadApps(['extended_authentication']);
    $appManager->loadApps(['filesystem', 'logging']);

    if ($app === 'core') {
        $resolvedFile = \OC::$SERVERROOT . '/' . $file;
    } else {
        if (!$appManager->isEnabledForUser($app)) {
            http_response_code(503);
            return;
        }
        $appManager->loadApp($app);
        $resolvedFile = $appManager->getAppPath($app) . '/' . ($parts[1] ?? '');
    }

    // $baseuri is expected by SabreDAV bootstrap files included below.
    $baseuri = \OC::$WEBROOT . '/remote.php/' . $service . '/';
    if (!file_exists($resolvedFile)) {
        http_response_code(404);
        return;
    }
    require_once $resolvedFile;
}

/**
 * Route a public.php-destined request to the appropriate app file.
 *
 * Mirrors the service-resolution logic in {NC_ROOT}/public.php.  Public
 * share WebDAV/DAV services are handled here; Rust's native DAV layer does
 * not currently handle public.php routes (they fall through to PHP-FPM).
 *
 * @param string $ncRoot Absolute path to the Nextcloud installation root.
 */
function route_public_php(string $ncRoot): void
{
    header("Content-Security-Policy: default-src 'none';");

    $services = [
        'webdav' => 'dav/appinfo/v1/publicwebdav.php',
        'dav'    => 'dav/appinfo/v2/publicremote.php',
    ];

    $request = \OCP\Server::get(\OCP\IRequest::class);
    $pathInfo = $request->getPathInfo();
    if ($pathInfo === false || $pathInfo === '') {
        http_response_code(404);
        return;
    }

    if (!$pos = strpos((string)$pathInfo, '/', 1)) {
        $pos = strlen((string)$pathInfo);
    }
    $service = substr((string)$pathInfo, 1, $pos - 1);

    $file = $services[$service] ?? \OCP\Server::get(\OCP\IConfig::class)->getAppValue('core', 'remote_' . $service);
    if (!$file) {
        http_response_code(404);
        return;
    }

    $file = ltrim($file, '/');
    $parts = explode('/', $file, 2);
    $app = $parts[0];

    $appManager = \OCP\Server::get(\OCP\App\IAppManager::class);
    \OC::$REQUESTEDAPP = $app;
    $appManager->loadApps(['authentication']);
    $appManager->loadApps(['extended_authentication']);
    $appManager->loadApps(['filesystem', 'logging']);

    if (!$appManager->isEnabledForUser($app)) {
        http_response_code(503);
        return;
    }
    $appManager->loadApp($app);
    $resolvedFile = $appManager->getAppPath($app) . '/' . ($parts[1] ?? '');

    $baseuri = \OC::$WEBROOT . '/public.php/' . $service . '/';
    if (!file_exists($resolvedFile)) {
        http_response_code(404);
        return;
    }
    require_once $resolvedFile;
}
