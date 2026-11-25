package ai.oxidea.build;

import org.osgi.framework.BundleActivator;
import org.osgi.framework.BundleContext;

/** Registers the redirect on bundle start, so the first import is redirected too. */
public final class RedirectActivator implements BundleActivator {

    @Override
    public void start(BundleContext context) {
        Redirect.ensureListening();
    }

    @Override
    public void stop(BundleContext context) {}
}
