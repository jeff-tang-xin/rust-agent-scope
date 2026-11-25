package ai.oxidea.build;

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;

import org.eclipse.core.resources.ResourcesPlugin;
import org.eclipse.core.runtime.CoreException;
import org.eclipse.core.runtime.IPath;
import org.eclipse.core.runtime.IProgressMonitor;
import org.eclipse.core.runtime.IStatus;
import org.eclipse.core.runtime.NullProgressMonitor;
import org.eclipse.core.runtime.Path;
import org.eclipse.core.runtime.Status;
import org.eclipse.core.runtime.jobs.Job;
import org.eclipse.jdt.apt.core.util.AptConfig;
import org.eclipse.jdt.core.ElementChangedEvent;
import org.eclipse.jdt.core.IClasspathEntry;
import org.eclipse.jdt.core.IJavaProject;
import org.eclipse.jdt.core.JavaCore;
import org.eclipse.jdt.core.JavaModelException;

/**
 * Redirects ECJ build outputs and APT generated sources of Maven-layout projects into an
 * IDE-owned staging tree ({@code .../target/oxidea-ide/...}). The annotation-processing
 * build cycle deletes and regenerates its outputs on every build with no transaction
 * protection; kept out of the shared {@code target/} tree, a failed or cancelled cycle can
 * no longer remove classes that terminal Maven builds wrote or running JVMs read. The host
 * publishes a successful staging build into {@code target/} (atomic per-file renames)
 * before a launch.
 *
 * Applied idempotently on every Java model change: m2e re-imports reset the classpath from
 * the pom and this re-applies the redirect, while an already-redirected project is left
 * untouched so the listener converges instead of looping.
 */
public final class Redirect {

    /** Path segment inserted after {@code target} for every redirected output. */
    public static final String STAGING = "oxidea-ide";

    private static volatile boolean listening = false;

    private Redirect() {}

    /** Applies the redirect on every Java model change; registered once, from either entry point. */
    public static synchronized void ensureListening() {
        if (listening) return;
        JavaCore.addElementChangedListener(event -> scheduleApply(), ElementChangedEvent.POST_CHANGE);
        listening = true;
    }

    /**
     * POST_CHANGE fires inside the resource-change broadcast, where the resource tree is
     * locked for modifications: mutating the classpath there fails with code 380 ("The
     * resource tree is locked"). Defers to a plain job, which runs outside the batch and
     * lets the Java model acquire the workspace lock itself.
     */
    private static void scheduleApply() {
        if (!pending.compareAndSet(false, true)) return;
        new Job("oxidea build redirect") {
            @Override
            protected IStatus run(IProgressMonitor monitor) {
                pending.set(false);
                applyAll();
                return Status.OK_STATUS;
            }
        }.schedule();
    }

    private static final AtomicBoolean pending = new AtomicBoolean();

    /** Redirects every workspace project that needs it; returns the projects the redirect is in effect for. */
    public static List<String> applyAll() {
        List<String> changed = new ArrayList<>();
        try {
            for (IJavaProject project : JavaCore.create(ResourcesPlugin.getWorkspace().getRoot()).getJavaProjects()) {
                if (apply(project)) changed.add(project.getElementName());
            }
        } catch (JavaModelException e) {
            // The model is mid-change; the next POST_CHANGE event retries.
        }
        return changed;
    }

    /**
     * Redirects one project's default output, per-source-folder outputs and APT generated
     * sources when they live under a {@code target} tree. Returns whether the redirect is
     * in effect for the project — applied by this call or already in place — so callers
     * see a stable answer regardless of which entry point (listener job or command) got
     * there first.
     */
    public static boolean apply(IJavaProject project) {
        try {
            IPath output = project.getOutputLocation();
            IPath staging = stagingPath(output);
            if (staging == null) return false; // not a target/ layout (Gradle, Eclipse-style): leave it alone
            boolean outputStaged = staging.equals(output);
            List<IClasspathEntry> entries = new ArrayList<>(List.of(project.getRawClasspath()));
            boolean changed = redirectEntryOutputs(entries);
            changed |= redirectGenSources(project, entries, AptConfig.getGenSrcDir(project), false);
            changed |= redirectGenSources(project, entries, AptConfig.getGenTestSrcDir(project), true);
            if (outputStaged && !changed) return true; // already applied: converge, no event loop
            project.setRawClasspath(entries.toArray(new IClasspathEntry[0]), staging, new NullProgressMonitor());
            return true;
        } catch (CoreException e) {
            // The model is mid-change or the project is closing; the next event retries.
            return false;
        }
    }

    /** {@code .../target/x} → {@code .../target/oxidea-ide/x}; {@code null} when the path has no {@code target} segment. */
    static IPath stagingPath(IPath path) {
        if (path == null) return null;
        String staged = staging(path.toPortableString());
        return staged == null ? null : Path.fromPortableString(staged);
    }

    /** The string form of {@link #stagingPath}: {@code .../target/x} → {@code .../target/oxidea-ide/x}. */
    static String staging(String path) {
        if (path == null || path.isEmpty()) return null;
        String norm = path.replace('\\', '/');
        String[] segments = norm.split("/");
        for (int i = 0; i < segments.length; i++) {
            if ("target".equals(segments[i])) {
                if (i + 1 < segments.length && STAGING.equals(segments[i + 1])) return path;
                StringBuilder sb = new StringBuilder();
                if (norm.startsWith("/")) sb.append('/');
                for (int j = 0; j < segments.length; j++) {
                    if (j > 0) sb.append('/');
                    if (j == i + 1) sb.append(STAGING).append('/');
                    sb.append(segments[j]);
                }
                return sb.toString();
            }
        }
        return null;
    }

    /**
     * Moves one APT generated-sources directory (main or test) with the outputs and keeps it
     * a source folder, so the model, index and navigation still see the generated
     * implementations. The pom's own generated-sources entry is dropped: that tree is
     * terminal Maven's territory now.
     */
    private static boolean redirectGenSources(IJavaProject project, List<IClasspathEntry> entries, String oldGenSrc, boolean test) {
        String genSrc = staging(oldGenSrc);
        if (genSrc == null || genSrc.equals(oldGenSrc)) return false;
        if (test) AptConfig.setGenTestSrcDir(project, genSrc);
        else AptConfig.setGenSrcDir(project, genSrc);
        boolean changed = dropSourceEntry(entries, folder(project, oldGenSrc));
        IPath newFolder = folder(project, genSrc);
        if (!hasSourceEntry(entries, newFolder)) {
            entries.add(JavaCore.newSourceEntry(newFolder));
            changed = true;
        }
        return changed;
    }

    /** A genSrcDir setting as the workspace-relative folder the classpath entries spell. */
    private static IPath folder(IJavaProject project, String genSrc) {
        if (genSrc == null || genSrc.isEmpty()) return null;
        Path p = new Path(genSrc.replace('\\', '/'));
        return p.isAbsolute() ? p : project.getPath().append(p);
    }

    /** Rewrites per-source-folder outputs ({@code target/test-classes} and friends) into the staging tree. */
    private static boolean redirectEntryOutputs(List<IClasspathEntry> entries) {
        boolean changed = false;
        for (int i = 0; i < entries.size(); i++) {
            IClasspathEntry entry = entries.get(i);
            if (entry.getEntryKind() != IClasspathEntry.CPE_SOURCE || entry.getOutputLocation() == null) continue;
            IPath staged = stagingPath(entry.getOutputLocation());
            if (staged == null || staged.equals(entry.getOutputLocation())) continue;
            entries.set(i, JavaCore.newSourceEntry(entry.getPath(), entry.getInclusionPatterns(),
                    entry.getExclusionPatterns(), staged, entry.getExtraAttributes()));
            changed = true;
        }
        return changed;
    }

    private static boolean dropSourceEntry(List<IClasspathEntry> entries, IPath path) {
        if (path == null) return false;
        return entries.removeIf(e -> e.getEntryKind() == IClasspathEntry.CPE_SOURCE && e.getPath().equals(path));
    }

    private static boolean hasSourceEntry(List<IClasspathEntry> entries, IPath path) {
        return entries.stream().anyMatch(e -> e.getEntryKind() == IClasspathEntry.CPE_SOURCE && e.getPath().equals(path));
    }
}
