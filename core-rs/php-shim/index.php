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
 *   2. require base.php — base.php calls OC::init() at file scope, which sets
 *      up the full DI container and services from config.php.  No explicit
 *      OC::init() call is needed here (and would break things if added, because
 *      a second OC::init() call makes require_once return true instead of the
 *      ClassLoader, causing a TypeError on the typed static property).
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

// ── __session_resolve — must run BEFORE the security gate and base.php ───────
// The session-resolve endpoint is an internal channel used by the Rust auth
// middleware (§7.9.3).  It must be dispatched before the normal bootstrap
// because:
//   1. It has its own trust check (HTTP_X_NC_PROXIED=1 only — no HTTP_X_NC_USER).
//   2. It needs to parse raw cookie bytes into $_COOKIE BEFORE base.php runs,
//      so that OC::init() → initSession() → CryptoWrapper reads oc_sessionPassphrase
//      from $_COOKIE, and IRequest::getCookie() can find the {instanceid} cookie.
//   3. It must not re-run the normal user-injection path.
if (($_SERVER['NC_ORIGINAL_SCRIPT'] ?? '') === '__session_resolve') {
    session_resolve_handler();
    exit(0);
}

// ── Security gate — must be the very first action ────────────────────────────
// Reject any FastCGI request that did not arrive through the Rust auth layer.
// This is the last line of defence; the primary control is the 0600 socket.
// Returns true when authenticated, false for whitelisted unauthenticated probes.
$_NC_IS_AUTHENTICATED = reject_unauthenticated_shim_request();

// ── Locate Nextcloud SERVERROOT ───────────────────────────────────────────────
// The shim can be deployed in two ways:
// 1. Inside NC root at core-rs/php-shim/index.php (development)
// 2. In a separate location like /usr/local/share/nc-server/php-shim/index.php (Docker)
//
// The Rust proxy passes NC_ROOT as the Nextcloud root directory.
// Fall back to NC_ORIGINAL_SCRIPT or dirname(__DIR__, 2) if not available.
$_NC_ROOT = resolve_nc_root();

// ── Fix SCRIPT_FILENAME for OC::init() path calculations ─────────────────────
// OC::init() → initPaths() computes OC::$SUBURI as:
//   substr(realpath(SCRIPT_FILENAME), strlen(SERVERROOT))
// If SCRIPT_FILENAME points to the shim (core-rs/php-shim/index.php), $SUBURI
// becomes "/core-rs/php-shim/index.php" which breaks $WEBROOT derivation.
// Override SCRIPT_FILENAME to the original entry-point so initPaths() sees
// e.g. "/var/www/html/index.php" and computes $SUBURI = "/index.php" correctly.
$_SERVER['SCRIPT_FILENAME'] = $_SERVER['NC_ORIGINAL_SCRIPT'] ?? $_NC_ROOT . '/index.php';

// ── Load the Nextcloud framework ──────────────────────────────────────────────
// versioncheck.php only outputs a friendly error if PHP is too old, then dies.
// base.php bootstraps the full DI container (autoloaders, config, server boot).
// OC::init() inside base.php sets OC::$SERVERROOT from __DIR__ of base.php,
// which correctly yields $_NC_ROOT — no manual override needed.
require_once $_NC_ROOT . '/lib/versioncheck.php';
require_once $_NC_ROOT . '/lib/base.php'; // base.php calls OC::init() at file scope

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
    // ── index.php — standard Nextcloud entry point ─────────────────────────────
    // OC::handleRequest() resolves the path through Symfony routing and
    // dispatches to the correct app controller.  Because setVolatileActiveUser()
    // was called above, handleRequest() skips the PHP-side login step entirely.
    case 'index.php':
        OC::handleRequest();
        break;

    // ── OCS entry points ──────────────────────────────────────────────────────
    // OCS routes are registered with a '/ocsapp' prefix in the Symfony router,
    // so they must go through Router::match('/ocsapp' + pathInfo), NOT through
    // OC::handleRequest() which omits that prefix.  Mirror ocs/v1.php logic.
    case 'v1.php':    // /ocs/v1.php/...
    case 'v2.php':    // /ocs/v2.php/...
        route_ocs_php();
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
 * Route an OCS API request (v1.php / v2.php) through the Symfony router.
 *
 * OCS routes are registered with the '/ocsapp' prefix, so
 * Router::match('/ocsapp' . $pathInfo) is required.  OC::handleRequest() uses
 * Router::match($pathInfo) without the prefix and would never match OCS routes.
 *
 * Mirrors the logic in {NC_ROOT}/ocs/v1.php.
 */
function route_ocs_php(): void
{
    if (\OCP\Util::needUpgrade()
        || \OCP\Server::get(\OCP\IConfig::class)->getSystemValueBool('maintenance')) {
        \OC\OCS\ApiHelper::respond(503, 'Service unavailable', ['X-Nextcloud-Maintenance-Mode' => '1'], 503);
        return;
    }

    try {
        $appManager = \OCP\Server::get(\OCP\App\IAppManager::class);
        $appManager->loadApps(['session']);
        $appManager->loadApps(['authentication']);
        $appManager->loadApps(['extended_authentication']);
        $appManager->loadApps();

        $request = \OCP\Server::get(\OCP\IRequest::class);
        $request->throwDecodingExceptionIfAny();

        if (!\OCP\Server::get(\OCP\IUserSession::class)->isLoggedIn()) {
            OC::handleLogin($request);
        }

        $matchUrl = '/ocsapp' . $request->getRawPathInfo();
        $isLoggedIn = \OCP\Server::get(\OCP\IUserSession::class)->isLoggedIn();
        $userId = \OCP\Server::get(\OCP\IUserSession::class)->getUser()?->getUID() ?? 'null';
        error_log("OCS DEBUG: matchUrl=$matchUrl loggedIn=" . ($isLoggedIn ? '1' : '0') . " userId=$userId SCRIPT_NAME=" . ($_SERVER['SCRIPT_NAME'] ?? 'unset') . " REQUEST_URI=" . ($_SERVER['REQUEST_URI'] ?? 'unset'));
        \OCP\Server::get(\OC\Route\Router::class)->match($matchUrl);
    } catch (\OCP\Security\Bruteforce\MaxDelayReached $ex) {
        \OC\OCS\ApiHelper::respond(\OCP\AppFramework\Http::STATUS_TOO_MANY_REQUESTS, $ex->getMessage());
    } catch (\Symfony\Component\Routing\Exception\ResourceNotFoundException $e) {
        $txt = 'Invalid query, please check the syntax. API specifications are here:'
            . ' http://www.freedesktop.org/wiki/Specifications/open-collaboration-services.' . "\n";
        \OC\OCS\ApiHelper::respond(\OCP\AppFramework\OCSController::RESPOND_NOT_FOUND, $txt);
    } catch (\Symfony\Component\Routing\Exception\MethodNotAllowedException $e) {
        \OC\OCS\ApiHelper::setContentType();
        http_response_code(405);
    } catch (\OC\User\LoginException $e) {
        \OC\OCS\ApiHelper::respond(\OCP\AppFramework\OCSController::RESPOND_UNAUTHORISED, 'Unauthorised');
    } catch (\Exception $e) {
        \OCP\Server::get(\Psr\Log\LoggerInterface::class)->error($e->getMessage(), ['exception' => $e]);
        $txt = 'Internal Server Error' . "\n";
        try {
            if (\OCP\Server::get(\OC\SystemConfig::class)->getValue('debug', false)) {
                $txt .= $e->getMessage();
            }
        } catch (\Throwable $e) {
            // Just to be safe
        }
        \OC\OCS\ApiHelper::respond(\OCP\AppFramework\OCSController::RESPOND_SERVER_ERROR, $txt);
    }
}

/**
 * Resolve the Nextcloud installation root directory.
 *
 * Resolution order:
 *   1. NC_ROOT — set by the Rust proxy on every FastCGI request to the
 *      NC root (most reliable; works for both in-tree and out-of-tree shim).
 *   2. dirname(dirname(NC_ORIGINAL_SCRIPT)) — NC_ORIGINAL_SCRIPT is the
 *      absolute path of the original PHP entry point (e.g. /var/www/html/index.php),
 *      so two dirname() calls yield the NC root.
 *   3. dirname(__DIR__, 2) — compile-time fallback for the development layout
 *      where the shim lives at {NC_ROOT}/core-rs/php-shim/index.php.
 */
function resolve_nc_root(): string
{
    if (!empty($_SERVER['NC_ROOT'])) {
        return $_SERVER['NC_ROOT'];
    }
    if (!empty($_SERVER['NC_ORIGINAL_SCRIPT'])) {
        return dirname(dirname($_SERVER['NC_ORIGINAL_SCRIPT']));
    }
    return dirname(__DIR__, 2);
}

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

/**
 * Handle an internal session-identity resolution request (§7.9.3).
 *
 * Called ONLY when NC_ORIGINAL_SCRIPT == '__session_resolve'.  This path runs
 * BEFORE the normal security gate and BEFORE base.php, so the caller must
 * perform its own security check.
 *
 * Protocol
 * --------
 * Trust signal : HTTP_X_NC_PROXIED=1 must be present (injected by Rust proxy;
 *                any client-supplied version is stripped in §7.1).  Absence →
 *                HTTP 403.  There is no HTTP_X_NC_USER check — the whole point
 *                is to *resolve* an unauthenticated-looking request.
 *
 * Cookie handling : PHP-FPM populates $_SERVER['HTTP_COOKIE'] from the FastCGI
 *                   HTTP_COOKIE param but does NOT populate $_COOKIE from it.
 *                   We parse the raw cookie string into $_COOKIE here so that:
 *                     - session_start() (inside Internal::__construct) picks up
 *                       the {instanceid} session cookie and resumes the existing
 *                       session.
 *                     - CryptoWrapper reads oc_sessionPassphrase from IRequest::
 *                       getCookie() (populated from $_COOKIE at DI container
 *                       build time inside OC::init()).
 *                     - IRequest::getCookie() can find all cookies for
 *                       tryTokenLogin() and loginWithCookie().
 *
 * Auth chain     : base.php is included (triggers OC::init(), initSession()),
 *                  then OC::handleLogin() runs the full PHP auth chain:
 *                    tryTokenLogin   → reads {instanceid} cookie → session_id()
 *                                      → SHA-512(id || secret) → oc_authtoken
 *                    loginWithCookie → reads nc_username/nc_token/nc_session_id
 *                                      → oc_preferences; rotates nc_token;
 *                                      emits new Set-Cookie headers via PHP's
 *                                      setcookie() which writes into FastCGI
 *                                      stdout headers automatically
 *                    tryBasicAuthLogin → no Authorization header → skipped
 *
 * SameSite check : performSameSiteCookieProtection() in base.php checks
 *                  basename($request->getScriptName()) and skips the check
 *                  when it is 'index.php'.  We set SCRIPT_NAME to '/index.php'
 *                  so base.php skips its SameSite re-check — verifying cookie
 *                  guards is Rust's responsibility on this internal path.
 *
 * Side effects   : If the remember-me path succeeds, loginWithCookie() calls
 *                  session_regenerate_id() (via Internal::regenerateId()) and
 *                  setMagicInCookie() which emits Set-Cookie headers for
 *                  nc_username, nc_token, nc_session_id.  These appear in the
 *                  FastCGI stdout header block and are forwarded to the Rust
 *                  caller (§7.9.4), which injects them into the real HTTP
 *                  response so the browser receives the rotated tokens.
 *
 * Response       : HTTP 200, Content-Type: application/json; charset=UTF-8.
 *                  Body: {"uid":"alice","dav_authenticated_uid":"alice"}
 *                  or    {"uid":null}   when no auth path succeeded.
 *
 * Security       : This endpoint must never be reachable via a public HTTP
 *                  route.  It is only callable internally by the Rust auth
 *                  middleware via the Unix FastCGI socket.  The 0600 socket
 *                  permission is the primary control; this check is defence-
 *                  in-depth.
 */
function session_resolve_handler(): void
{
    // ── Silence PHP error output ─────────────────────────────────────────────
    // ob_start() below captures echo/print output but does NOT intercept
    // xdebug in develop mode: xdebug writes E_WARNING HTML directly to the
    // FastCGI stdout stream (via the SAPI's ub_write hook), bypassing all
    // output buffers.  When that happens PHP auto-sends a text/html header,
    // xdebug HTML lands in the CGI body, and the Rust caller's JSON parser
    // fails on "<br /><table class='xdebug-error …'.  Suppressing both
    // display_errors and html_errors here ensures the response body is always
    // clean JSON regardless of the environment's xdebug/PHP config.
    ini_set('display_errors', '0');
    ini_set('html_errors', '0');

    // ── Trust check ──────────────────────────────────────────────────────────
    // HTTP_X_NC_PROXIED=1 is injected by the Rust proxy on every proxied
    // request and stripped from any client-supplied X-NC-Proxied header.
    // A missing or incorrect value means the connection bypassed the Rust proxy.
    if (($_SERVER['HTTP_X_NC_PROXIED'] ?? '') !== '1') {
        header('Status: 403 Forbidden');
        header('Content-Type: text/plain; charset=UTF-8');
        echo "403 Forbidden: request did not pass Rust proxy layer\n";
        return;
    }

    // ── Parse raw Cookie header into $_COOKIE ────────────────────────────────
    // PHP-FPM sets $_SERVER['HTTP_COOKIE'] from the FastCGI HTTP_COOKIE param
    // but does NOT populate $_COOKIE from it.  We must do this BEFORE base.php
    // is included because OC::init() → initSession() → CryptoWrapper and
    // Internal::__construct() all read from $_COOKIE (directly or via
    // IRequest::getCookie() which is built from $_COOKIE at DI init time).
    //
    // We use urldecode() on both name and value to match PHP's native behaviour
    // when it parses a real Cookie: header into $_COOKIE.
    $rawCookie = $_SERVER['HTTP_COOKIE'] ?? '';
    if ($rawCookie !== '') {
        foreach (explode(';', $rawCookie) as $pair) {
            $pair = trim($pair);
            if ($pair === '') {
                continue;
            }
            $eqPos = strpos($pair, '=');
            if ($eqPos === false) {
                // Cookie with no value — store as empty string.
                $name = urldecode(trim($pair));
                if ($name !== '') {
                    $_COOKIE[$name] = '';
                }
                continue;
            }
            $name  = urldecode(trim(substr($pair, 0, $eqPos)));
            $value = urldecode(trim(substr($pair, $eqPos + 1)));
            if ($name !== '') {
                $_COOKIE[$name] = $value;
            }
        }
    }

    // ── SCRIPT_FILENAME / SCRIPT_NAME override ───────────────────────────────
    // OC::init() → initPaths() derives $SUBURI from SCRIPT_FILENAME.
    // performSameSiteCookieProtection() skips the check when basename of
    // $request->getScriptName() === 'index.php'.  Use index.php to ensure
    // base.php does not run its SameSite check on this internal path.
    //
    // NC_ROOT is the Nextcloud root passed by the Rust proxy in the
    // session-resolve FastCGI params.  Fall back to NC_ORIGINAL_SCRIPT or
    // dirname(__DIR__, 2) — see resolve_nc_root().
    $ncRoot = resolve_nc_root();
    $_SERVER['SCRIPT_FILENAME'] = $ncRoot . '/index.php';
    $_SERVER['SCRIPT_NAME']     = '/index.php';

    // ── Bootstrap ────────────────────────────────────────────────────────────
    // base.php calls OC::init() at file scope which sets up the DI container,
    // session (initSession → Internal → CryptoWrapper), and all OCP services.
    //
    // Capture all output during bootstrap so that PHP warnings, xdebug HTML
    // error blocks, or any other incidental output do not pollute the JSON
    // response body.  The buffer is discarded before we emit JSON below.
    ob_start();
    require_once $ncRoot . '/lib/versioncheck.php';
    require_once $ncRoot . '/lib/base.php';

    // ── Run the PHP auth chain ───────────────────────────────────────────────
    // OC::handleLogin() mirrors base.php:1225-1255 (PHP source of truth):
    //   1. tryTokenLogin    — reads {instanceid} cookie value as session_id(),
    //                         computes SHA-512(id || server_secret), looks up
    //                         oc_authtoken.token.
    //   2. loginWithCookie  — requires ALL THREE: nc_username, nc_token,
    //                         nc_session_id; validates nc_token against
    //                         oc_preferences; rotates the token; calls
    //                         session_regenerate_id(); emits Set-Cookie headers.
    //   3. tryBasicAuthLogin — no Authorization header present → skipped.
    // We do NOT call setVolatileActiveUser() here; the whole point is to let
    // PHP authenticate the user from the cookies it received.
    $request = \OCP\Server::get(\OCP\IRequest::class);
    OC::handleLogin($request);

    // ── Read resolved identity ───────────────────────────────────────────────
    $userSession = \OCP\Server::get(\OCP\IUserSession::class);
    $uid = $userSession->getUser()?->getUID();

    // AUTHENTICATED_TO_DAV_BACKEND is a UID string set by
    // apps/dav/lib/Connector/Sabre/Auth.php:91 on the first successful DAV
    // request in this session.  Null when absent (Session.php stores null
    // for missing keys).
    $session = \OCP\Server::get(\OCP\ISession::class);
    $davAuth = $session->get('AUTHENTICATED_TO_DAV_BACKEND');

    // ── Emit JSON response ───────────────────────────────────────────────────
    // Discard anything that leaked into the output buffer during bootstrap
    // (PHP warnings, xdebug HTML blocks, etc.) so the response body contains
    // only valid JSON.
    // Any Set-Cookie headers emitted by loginWithCookie() side effects are
    // already in the FastCGI stdout header block — no explicit action needed.
    // The Rust caller (§7.9.4) reads them from the response and injects them
    // into the real HTTP response it sends to the browser.
    ob_end_clean();
    header('Content-Type: application/json; charset=UTF-8');
    echo json_encode(
        [
            'uid'                   => $uid,
            'dav_authenticated_uid' => $davAuth,
        ],
        JSON_UNESCAPED_UNICODE | JSON_UNESCAPED_SLASHES
    );
}
