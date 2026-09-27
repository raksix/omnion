"use client";

/**
 * The browser half of a passkey ceremony (REQ-006, slice 3b).
 *
 * The API hands over the options (`PublicKeyCredentialCreationOptions` /
 * `PublicKeyCredentialRequestOptions` shapes) and this module runs `navigator.credentials.*`
 * for real, then serialises the answer the way the API expects: the client data as the JSON
 * text the ceremony signs, every binary part as base64url.
 *
 * Nothing here decides anything: the server verifies the challenge, the origin and the
 * signature, so a browser that lies about any of them gets a refusal rather than a factor.
 */
import type {
  PasskeyCreationOptions,
  PasskeyCredential,
  PasskeyRequestOptions,
} from "./api";

/** base64url (unpadded) of an ArrayBuffer — the wire form for every binary part. */
export function toBase64Url(value: ArrayBuffer): string {
  const bytes = new Uint8Array(value);
  let binary = "";
  for (const byte of bytes) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** Decode base64url back into bytes (backed by its own `ArrayBuffer`, which is the shape the
 * WebAuthn APIs accept). */
export function fromBase64Url(text: string): Uint8Array<ArrayBuffer> {
  const padded = text.replace(/-/g, "+").replace(/_/g, "/");
  const binary = atob(padded + "=".repeat((4 - (padded.length % 4)) % 4));
  const bytes = new Uint8Array(new ArrayBuffer(binary.length));
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

/** Whether this browser can run a ceremony at all. */
export function passkeysSupported(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof window.PublicKeyCredential !== "undefined" &&
    typeof navigator !== "undefined" &&
    typeof navigator.credentials?.create === "function"
  );
}

/** The transports a credential reports, when the browser has the helper. */
async function transportsOf(credential: PublicKeyCredential): Promise<string[]> {
  const helper = (
    PublicKeyCredential as unknown as {
      getTransports?: (attachment: AuthenticatorAttachment | null) => string[];
    }
  ).getTransports;
  if (typeof helper !== "function") {
    return [];
  }
  try {
    return helper.call(
      PublicKeyCredential,
      (credential.authenticatorAttachment ?? null) as AuthenticatorAttachment | null,
    );
  } catch {
    return [];
  }
}

/** Run `navigator.credentials.create` for a registration ceremony. */
export async function createPasskey(
  options: PasskeyCreationOptions,
): Promise<PasskeyCredential> {
  const publicKey: PublicKeyCredentialCreationOptions = {
    challenge: fromBase64Url(options.challenge),
    rp: { id: options.rp.id, name: options.rp.name },
    user: {
      id: fromBase64Url(options.user.id),
      name: options.user.name,
      displayName: options.user.displayName,
    },
    pubKeyCredParams: options.pubKeyCredParams.map((entry) => ({
      type: "public-key" as PublicKeyCredentialType,
      alg: entry.alg,
    })),
    timeout: options.timeout,
    attestation: options.attestation as AttestationConveyancePreference,
    authenticatorSelection: {
      residentKey: options.authenticatorSelection
        .residentKey as ResidentKeyRequirement,
      userVerification: options.authenticatorSelection
        .userVerification as UserVerificationRequirement,
    },
    excludeCredentials: options.excludeCredentials.map((entry) => ({
      type: "public-key" as PublicKeyCredentialType,
      id: fromBase64Url(entry.id),
    })),
  };

  const credential = (await navigator.credentials.create({ publicKey })) as
    | PublicKeyCredential
    | null;
  if (!credential) {
    throw new Error("The authenticator did not answer the ceremony.");
  }

  const response = credential.response as AuthenticatorAttestationResponse;
  return {
    id: credential.id,
    client_data_json: new TextDecoder().decode(response.clientDataJSON),
    attestation_object: toBase64Url(response.attestationObject),
    transports: await transportsOf(credential),
  };
}

/** Run `navigator.credentials.get` for a sign-in ceremony. */
export async function getPasskeyAssertion(
  options: PasskeyRequestOptions,
): Promise<PasskeyCredential> {
  const publicKey: PublicKeyCredentialRequestOptions = {
    challenge: fromBase64Url(options.challenge),
    rpId: options.rpId,
    allowCredentials: options.allowCredentials.map((entry) => ({
      type: "public-key" as PublicKeyCredentialType,
      id: fromBase64Url(entry.id),
      transports: entry.transports as AuthenticatorTransport[] | undefined,
    })),
    timeout: options.timeout,
    userVerification: options.userVerification as UserVerificationRequirement,
  };

  const credential = (await navigator.credentials.get({ publicKey })) as
    | PublicKeyCredential
    | null;
  if (!credential) {
    throw new Error("The authenticator did not answer the ceremony.");
  }

  const response = credential.response as AuthenticatorAssertionResponse;
  return {
    id: credential.id,
    client_data_json: new TextDecoder().decode(response.clientDataJSON),
    authenticator_data: toBase64Url(response.authenticatorData),
    signature: toBase64Url(response.signature),
  };
}

/** A reader-facing sentence for a ceremony failure the browser raised. */
export function ceremonyMessage(cause: unknown): string {
  if (cause instanceof DOMException) {
    if (cause.name === "NotAllowedError") {
      return "The ceremony was dismissed or timed out — try again.";
    }
    if (cause.name === "InvalidStateError") {
      return "This authenticator is already enrolled on this account.";
    }
    if (cause.name === "NotSupportedError") {
      return "This browser cannot run a passkey ceremony.";
    }
    return cause.message;
  }
  return cause instanceof Error ? cause.message : "The ceremony did not complete.";
}
