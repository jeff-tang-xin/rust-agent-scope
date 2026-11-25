package ai.oxidea.build;

import java.util.List;
import java.util.Map;

import org.eclipse.core.runtime.IProgressMonitor;
import org.eclipse.jdt.ls.core.internal.IDelegateCommandHandler;

/**
 * Host-invoked redirect application: {@code oxidea.java.build.redirect} applies the
 * staging redirect to every workspace project and reports what changed. The bundle's own
 * model-change listener keeps projects redirected between calls; this command lets the
 * host apply and verify at the moments it cares about (server ready, before a launch).
 */
public final class RedirectCommand implements IDelegateCommandHandler {

    @Override
    public Object executeCommand(String command, List<Object> arguments, IProgressMonitor monitor) throws Exception {
        Redirect.ensureListening();
        return Map.of("redirected", Redirect.applyAll());
    }
}
