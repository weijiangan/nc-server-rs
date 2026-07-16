## Reference implementation locations

- Entry points: `index.php`, `remote.php`, `public.php`, `ocs/v1.php`, `ocs/v2.php`, `status.php`.
- OCS responses: `lib/private/AppFramework/OCS/*Response.php`, `lib/private/OCS/ApiHelper.php`.
- OCS capabilities: `lib/private/OCS/CoreCapabilities.php`, `core/AppInfo/Capabilities.php`.
- Security: `lib/private/AppFramework/Middleware/Security/SecurityMiddleware.php`.
- DAV servers: `apps/dav/appinfo/v1/*`, `apps/dav/appinfo/v2/*`, `apps/dav/lib/Server.php`.
- DAV file properties: `apps/dav/lib/Connector/Sabre/FilesPlugin.php`.
- Chunked upload v1: `apps/dav/lib/Upload/ChunkingPlugin.php`.
- Chunked upload v2: `apps/dav/lib/Upload/ChunkingV2Plugin.php`.
- Bulk upload: `apps/dav/lib/BulkUpload/BulkUploadPlugin.php`.
- DAV response headers: `apps/dav/lib/Connector/Sabre/CopyEtagHeaderPlugin.php`, `RequestIdHeaderPlugin.php`, `UserIdHeaderPlugin.php`.
- File search: `apps/dav/lib/Files/FileSearchBackend.php`.
- Quota enforcement: `apps/dav/lib/Connector/Sabre/QuotaPlugin.php`.
- Auth (DAV basic): `apps/dav/lib/Connector/Sabre/Auth.php`.
- Auth (DAV bearer): `apps/dav/lib/Connector/Sabre/BearerAuth.php`.
- Client quirks: `apps/dav/lib/Connector/Sabre/AnonymousOptionsPlugin.php`, `AppleQuirksPlugin.php`, `BlockLegacyClientPlugin.php`, `FakeLockerPlugin.php`.
- Files app REST: `apps/files/lib/Controller/ApiController.php`.
- Public sharing: `apps/files_sharing/lib/Controller/ShareController.php`.

---

Prev: [`14-configuration-values-influencing-api-behavior.md`](14-configuration-values-influencing-api-behavior.md) · Up: [`README.md`](README.md)
