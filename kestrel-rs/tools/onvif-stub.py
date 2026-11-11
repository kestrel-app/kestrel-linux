#!/usr/bin/env python3
"""A minimal ONVIF device, enough to exercise Kestrel's connect path.

There is no ONVIF camera on the build machine, and the parts of the client most
likely to be wrong are the ones no unit test reaches: whether the WS-Security
digest is one a device would actually accept, and whether the timestamp inside
it is written in the device's clock rather than ours.

So this verifies the digest it is sent rather than accepting anything, and it
runs forty minutes ahead on purpose. A client that stamps Created from its own
clock authenticates against this stub exactly as badly as it would against a
real camera.

    python3 tools/onvif-stub.py 8081 &
    KESTREL_TEST_ONVIF=127.0.0.1:8081 KESTREL_TEST_ONVIF_USER=admin \
      KESTREL_TEST_ONVIF_PASS=s3cr3t ./build.sh test -- --ignored talks_to_a_real --nocapture

It answers as a three-camera NVR: five profiles over three video sources, so the
main/sub grouping in api::vendor::onvif has something real to get wrong.
"""
import base64, hashlib, re, sys, datetime
from http.server import BaseHTTPRequestHandler, HTTPServer

USER, PASSWORD = "admin", "s3cr3t"
# Deliberately skewed: the device thinks it is 40 minutes ahead. A client that
# stamps Created from its own clock gets refused by a real camera.
SKEW = datetime.timedelta(minutes=40)
seen = {"auth_ok": 0, "auth_bad": 0, "calls": []}

def text_of(body, name):
    # The tag must END at the name (or at a space before its attributes),
    # or "Username" also matches "<wsse:UsernameToken>".
    m = re.search(r"<(?:\w+:)?%s(?:\s[^>]*)?>(.*?)</(?:\w+:)?%s>" % (name, name), body, re.S)
    return m.group(1).strip() if m else ""

def check_auth(body):
    if "UsernameToken" not in body:
        return None
    user = text_of(body, "Username")
    digest = text_of(body, "Password")
    nonce = text_of(body, "Nonce")
    created = text_of(body, "Created")
    if not (user and digest and nonce and created):
        return False
    want = base64.b64encode(hashlib.sha1(
        base64.b64decode(nonce) + created.encode() + PASSWORD.encode()).digest()).decode()
    ok = (user == USER and digest == want)
    if not ok:
        print(f"  !! digest mismatch: got {digest} want {want} (user={user})", flush=True)
    return ok

ENV = ('<?xml version="1.0" encoding="UTF-8"?>'
       '<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope"'
       ' xmlns:tds="http://www.onvif.org/ver10/device/wsdl"'
       ' xmlns:trt="http://www.onvif.org/ver10/media/wsdl"'
       ' xmlns:tt="http://www.onvif.org/ver10/schema">'
       '<env:Body>%s</env:Body></env:Envelope>')

def profiles_body():
    out = []
    for i, (name, w, h, enc, ptz) in enumerate([
            ("Front Door", 2560, 1440, "H265", True),
            ("Front Door Sub", 640, 360, "H264", True),
            ("Drive", 1920, 1080, "H264", False),
            ("Drive Sub", 640, 360, "H264", False),
            ("Garage", 1920, 1080, "H264", False)]):
        src = {0: 1, 1: 1, 2: 2, 3: 2, 4: 3}[i]
        ptz_el = '<tt:PTZConfiguration token="PTZ_1"><tt:Name>ptz</tt:Name></tt:PTZConfiguration>' if ptz else ''
        out.append(
            f'<trt:Profiles token="Profile_{i}"><tt:Name>{name}</tt:Name>'
            f'<tt:VideoSourceConfiguration token="VSC_{src}">'
            f'<tt:SourceToken>VideoSource_{src}</tt:SourceToken>'
            f'<tt:Bounds x="0" y="0" width="4096" height="2160"/>'
            f'</tt:VideoSourceConfiguration>'
            f'<tt:VideoEncoderConfiguration token="VEC_{i}"><tt:Encoding>{enc}</tt:Encoding>'
            f'<tt:Resolution><tt:Width>{w}</tt:Width><tt:Height>{h}</tt:Height></tt:Resolution>'
            f'</tt:VideoEncoderConfiguration>{ptz_el}</trt:Profiles>')
    return "<trt:GetProfilesResponse>" + "".join(out) + "</trt:GetProfilesResponse>"

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0))).decode("utf-8", "replace")
        action = "unknown"
        for name in ("GetSystemDateAndTime", "GetDeviceInformation", "GetCapabilities",
                     "GetProfiles", "GetStreamUri", "GetSnapshotUri", "GetPresets",
                     "ContinuousMove", "Stop"):
            if name in body:
                action = name
                break
        auth = check_auth(body)
        seen["calls"].append(action)
        if auth is True:
            seen["auth_ok"] += 1
        elif auth is False:
            seen["auth_bad"] += 1
        print(f"-> {action:22} auth={ {True:'OK', False:'BAD', None:'none'}[auth] }", flush=True)

        # Everything but the clock read needs a good credential.
        if action != "GetSystemDateAndTime" and auth is not True:
            return self.reply(
                '<env:Fault xmlns:env="http://www.w3.org/2003/05/soap-envelope">'
                '<env:Code><env:Value>env:Sender</env:Value><env:Subcode>'
                '<env:Value>ter:NotAuthorized</env:Value></env:Subcode></env:Code>'
                '<env:Reason><env:Text>Sender not Authorized</env:Text></env:Reason>'
                '</env:Fault>', status=400)

        now = datetime.datetime.now(datetime.timezone.utc) + SKEW
        if action == "GetSystemDateAndTime":
            return self.reply(
                '<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime>'
                '<tt:DateTimeType>NTP</tt:DateTimeType><tt:UTCDateTime>'
                f'<tt:Time><tt:Hour>{now.hour}</tt:Hour><tt:Minute>{now.minute}</tt:Minute>'
                f'<tt:Second>{now.second}</tt:Second></tt:Time>'
                f'<tt:Date><tt:Year>{now.year}</tt:Year><tt:Month>{now.month}</tt:Month>'
                f'<tt:Day>{now.day}</tt:Day></tt:Date>'
                '</tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>')
        if action == "GetDeviceInformation":
            return self.reply(
                '<tds:GetDeviceInformationResponse>'
                '<tds:Manufacturer>Acme</tds:Manufacturer><tds:Model>AC-4200</tds:Model>'
                '<tds:FirmwareVersion>5.7.3</tds:FirmwareVersion>'
                '<tds:SerialNumber>SN12345</tds:SerialNumber>'
                '</tds:GetDeviceInformationResponse>')
        if action == "GetCapabilities":
            port = self.server.server_address[1]
            return self.reply(
                '<tds:GetCapabilitiesResponse><tds:Capabilities>'
                f'<tt:Media><tt:XAddr>http://192.0.2.77:80/onvif/media</tt:XAddr></tt:Media>'
                f'<tt:PTZ><tt:XAddr>http://192.0.2.77:80/onvif/ptz</tt:XAddr></tt:PTZ>'
                '</tds:Capabilities></tds:GetCapabilitiesResponse>')
        if action == "GetProfiles":
            return self.reply(profiles_body())
        if action == "GetStreamUri":
            token = text_of(body, "ProfileToken")
            return self.reply(
                '<trt:GetStreamUriResponse><trt:MediaUri>'
                f'<tt:Uri>rtsp://192.0.2.77:554/stream/{token}</tt:Uri>'
                '<tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>'
                '</trt:MediaUri></trt:GetStreamUriResponse>')
        if action == "GetSnapshotUri":
            token = text_of(body, "ProfileToken")
            return self.reply(
                '<trt:GetSnapshotUriResponse><trt:MediaUri>'
                f'<tt:Uri>http://192.0.2.77/snapshot/{token}</tt:Uri>'
                '</trt:MediaUri></trt:GetSnapshotUriResponse>')
        return self.reply("<tds:Ok/>")

    def reply(self, payload, status=200):
        out = (ENV % payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/soap+xml; charset=utf-8")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8081
    srv = HTTPServer(("127.0.0.1", port), Handler)
    print(f"ONVIF stub on 127.0.0.1:{port} (user={USER})", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    print(f"\nsummary: auth_ok={seen['auth_ok']} auth_bad={seen['auth_bad']}", flush=True)
