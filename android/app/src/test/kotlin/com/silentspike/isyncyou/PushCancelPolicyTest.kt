package com.silentspike.isyncyou

import org.junit.Assert.*
import org.junit.Test

class PushCancelPolicyTest {
    @Test fun onlyUncertainPrepareCanBeCancelled() {
        assertTrue(PushCancelPolicy.canClaim("prepare_unknown"))
        assertTrue(PushCancelPolicy.canClaim("prepare_cancel_unknown"))
        for (stage in listOf("active", "credentials_ready", "prepare_cancelled", "endpoint_received", "")) {
            assertFalse(PushCancelPolicy.canClaim(stage))
        }
    }
    @Test fun existingOrConflictingBindingIsNeverTreatedAsAbsent() {
        assertEquals("prepare_cancelled", PushCancelPolicy.confirmedStage("absent"))
        assertEquals("prepare_cancel_conflict", PushCancelPolicy.confirmedStage("present"))
        assertEquals("prepare_cancel_conflict", PushCancelPolicy.confirmedStage("conflict"))
        assertNull(PushCancelPolicy.confirmedStage("unknown"))
    }
    @Test fun detachmentAndReplacementRequireConfirmedPriorStages() {
        assertTrue(PushCancelPolicy.canDetach("prepare_cancelled"))
        assertFalse(PushCancelPolicy.canDetach("prepare_cancel_unknown"))
        assertFalse(PushCancelPolicy.canDetach("prepare_cancel_conflict"))
        assertTrue(PushCancelPolicy.canRegisterReplacement("distributor_detached"))
        assertFalse(PushCancelPolicy.canRegisterReplacement("distributor_unregister_unknown"))
        assertFalse(PushCancelPolicy.canRegisterReplacement("prepare_cancelled"))
    }
}
