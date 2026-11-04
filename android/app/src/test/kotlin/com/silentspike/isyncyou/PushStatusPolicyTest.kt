package com.silentspike.isyncyou

import org.junit.Assert.*
import org.junit.Test

class PushStatusPolicyTest {
    @Test fun uncertaintyAndFencesRemainVisibleDuringDegradation() {
        for (stage in listOf("prepare_unknown", "prepare_cancel_unknown", "distributor_unregister_unknown", "create_unknown", "verify_unknown", "revoke_unknown", "prepare_cancelled", "prepare_cancel_conflict", "distributor_detached", "replacement_revoke_retry_ready", "broker_bound_logout", "broker_revoked_logout")) {
            assertTrue(PushStatusPolicy.retainStage(stage))
        }
        for (stage in listOf("active", "registering", "endpoint_received", "credentials_ready", "")) {
            assertFalse(PushStatusPolicy.retainStage(stage))
        }
    }
}
